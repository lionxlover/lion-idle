#!/usr/bin/env python3
"""Minimal mock Wayland compositor for lion-idle's live integration tests.

Speaks just enough of the Wayland wire protocol to be an
ext-idle-notify-v1 server: wl_display.get_registry / sync,
wl_registry.global + bind, ext_idle_notifier_v1.get_idle_notification,
ext_idle_notification_v1.idled / resumed / destroy, wl_seat (inert).

Controlled from the test driver through a FIFO: lines of
  IDLE <timeout_ms>     -> send `idled` for the notification with that timeout
  RESUME <timeout_ms>   -> send `resumed` for that notification
  QUIT                  -> close the client connection and exit
  PING                  -> log a marker (readiness/ordering probes)

The client (lion-idle) is expected to (re)create notifications as its
thresholds change; timeouts map 1:1 to thresholds, so the driver can
target whichever threshold it wants to fire.

Wayland wire format: every message is
  [u32 object_id][u32 (size<<16 | opcode)]  (size includes the 8-byte header)
followed by u32-arg payloads; strings are [u32 len_with_nul][bytes][pad4].
"""

import os
import struct
import sys
import threading
import socketserver
import socket
import time

SOCKET_PATH = sys.argv[1]
FIFO_PATH = sys.argv[2]
LOG_PATH = sys.argv[3]

def log(msg):
    with open(LOG_PATH, "a") as f:
        f.write(f"{time.time():.3f} {msg}\n")


class Client:
    def __init__(self, sock):
        self.sock = sock
        self.buf = b""
        self.registry_id = None
        self.notifier_id = None
        # notification object id -> timeout ms; timeout -> latest object id
        self.notifications = {}
        self.by_timeout = {}

    # ---- framing ----
    def send(self, obj_id, opcode, payload=b""):
        size = 8 + len(payload)
        self.sock.sendall(struct.pack("<II", obj_id, (size << 16) | opcode) + payload)

    def recv_messages(self):
        """Yield (obj_id, opcode, args_bytes) for every complete message."""
        while True:
            while len(self.buf) < 8:
                data = self.sock.recv(65536)
                if not data:
                    return
                self.buf += data
            obj_id, packed = struct.unpack("<II", self.buf[:8])
            size = packed >> 16
            opcode = packed & 0xFFFF
            while len(self.buf) < size:
                data = self.sock.recv(65536)
                if not data:
                    return
                self.buf += data
            args = self.buf[8:size]
            self.buf = self.buf[size:]
            yield obj_id, opcode, args

    # ---- protocol handling ----
    def handle(self, obj_id, opcode, args):
        if obj_id == 1 and opcode == 1:  # wl_display.get_registry
            (reg_id,) = struct.unpack("<I", args[:4])
            self.advertise_globals(reg_id)
            self.registry_id = reg_id
        elif obj_id == 1 and opcode == 0:  # wl_display.sync
            (cb_id,) = struct.unpack("<I", args[:4])
            self.send(cb_id, 0, struct.pack("<I", 0))  # wl_callback.done
        elif self.registry_id is not None and obj_id == self.registry_id and opcode == 0:
            # wl_registry.bind(name, interface, version, id)
            off = 0
            (name,) = struct.unpack_from("<I", args, off); off += 4
            (slen,) = struct.unpack_from("<I", args, off); off += 4
            iface = args[off:off + slen - 1].decode()
            off += slen + ((-slen) % 4)
            (version, new_id) = struct.unpack_from("<II", args, off)
            log(f"bind name={name} iface={iface} v{version} id={new_id}")
            if iface == "ext_idle_notifier_v1":
                self.notifier_id = new_id
            # wl_seat: inert; the client never needs its events
        elif self.notifier_id is not None and obj_id == self.notifier_id and opcode == 1:
            # ext_idle_notifier_v1.get_idle_notification(new_id, timeout, seat)
            (new_id, timeout, _seat) = struct.unpack("<III", args[:12])
            self.notifications[new_id] = timeout
            self.by_timeout[timeout] = new_id
            log(f"notification id={new_id} timeout={timeout}ms")
        else:
            # ext_idle_notification_v1.destroy (opcode 0) and anything
            # else: ignore -- destroying on reconfigure/rearm is expected.
            pass

    def advertise_globals(self, reg_id):
        # wl_registry.global(name, interface, version)
        for name, iface, ver in [
            (1, "wl_seat", 7),
            (2, "ext_idle_notifier_v1", 1),
        ]:
            b = iface.encode() + b"\0"
            pad = (-len(b)) % 4
            payload = (
                struct.pack("<I", name)
                + struct.pack("<I", len(b))
                + b
                + b"\0" * pad
                + struct.pack("<I", ver)
            )
            self.send(reg_id, 0, payload)
            log(f"global {iface} v{ver}")

    def fire_idle(self, timeout):
        nid = self.by_timeout.get(timeout)
        if nid is None:
            log(f"IDLE {timeout}: no such notification (have {list(self.by_timeout)})")
            return False
        self.send(nid, 0, b"")  # idled
        log(f"-> idled timeout={timeout}")
        return True

    def fire_resume(self, timeout):
        nid = self.by_timeout.get(timeout)
        if nid is None:
            log(f"RESUME {timeout}: no such notification (have {list(self.by_timeout)})")
            return False
        self.send(nid, 1, b"")  # resumed
        log(f"-> resumed timeout={timeout}")
        return True


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        client = Client(self.request)
        log("client connected")
        gen = client.recv_messages()
        # First message must be wl_display.get_registry.
        try:
            obj_id, opcode, args = next(gen)
        except StopIteration:
            log("client closed before get_registry")
            return
        if not (obj_id == 1 and opcode == 1):
            log(f"unexpected first message obj={obj_id} op={opcode}")
            return
        (reg_id,) = struct.unpack("<I", args[:4])
        client.advertise_globals(reg_id)
        client.registry_id = reg_id

        def reader():
            try:
                for obj_id, opcode, args in gen:
                    client.handle(obj_id, opcode, args)
            except (ConnectionResetError, OSError):
                pass
            log("client connection ended")

        threading.Thread(target=reader, daemon=True).start()

        # A FIFO read hits EOF whenever the last writer closes (every
        # `echo > fifo` from the driver); re-open and keep serving until
        # an explicit QUIT.
        quit_requested = False
        while not quit_requested:
            with open(FIFO_PATH, "r") as fifo:
                for line in fifo:
                    line = line.strip()
                    if not line:
                        continue
                    if line == "QUIT":
                        log("QUIT requested")
                        quit_requested = True
                        break
                    if line == "PING":
                        log("PING")
                        continue
                    cmd, _, arg = line.partition(" ")
                    arg = int(arg)
                    if cmd == "IDLE":
                        client.fire_idle(arg)
                    elif cmd == "RESUME":
                        client.fire_resume(arg)
        try:
            self.request.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        log("mock compositor session over")


class Server(socketserver.UnixStreamServer):
    allow_reuse_address = True


if __name__ == "__main__":
    for path in (SOCKET_PATH, FIFO_PATH):
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
    os.mkfifo(FIFO_PATH)
    srv = Server(SOCKET_PATH, Handler)
    log(f"mock compositor listening on {SOCKET_PATH}, fifo {FIFO_PATH}")
    try:
        srv.serve_forever()
    finally:
        for path in (SOCKET_PATH, FIFO_PATH):
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
