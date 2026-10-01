#!/usr/bin/env python3
"""Stay-alive D-Bus caller for lion-idle live tests.

gdbus exits after one call, which (correctly!) auto-releases any
inhibit it took via the daemon's peer-death cleanup. To test *held*
inhibitors, this process connects, inhibits, writes the cookie(s) to
an output file, and stays on the bus until killed.

Usage:
  inhibit_holder.py <bus_address> <surface> <app_id> <reason> <outfile> [count]

surface: "native" -> os.lionos.Idle1.Inhibit
         "screensaver" -> org.freedesktop.ScreenSaver.Inhibit
"""
import sys
import time

from jeepney import DBusAddress, MessageType, new_method_call
from jeepney.io.blocking import open_dbus_connection


def main():
    bus_addr, surface, app_id, reason, outfile = sys.argv[1:6]
    count = int(sys.argv[6]) if len(sys.argv) > 6 else 1

    if surface == "native":
        remote = DBusAddress("/os/lionos/Idle", bus_name="os.lionos.Idle",
                             interface="os.lionos.Idle1")
    elif surface == "screensaver":
        remote = DBusAddress("/org/freedesktop/ScreenSaver",
                             bus_name="org.freedesktop.ScreenSaver",
                             interface="org.freedesktop.ScreenSaver")
    else:
        raise SystemExit(f"unknown surface {surface}")
    method = "Inhibit"

    conn = open_dbus_connection(bus_addr)
    results = []
    for _ in range(count):
        msg = new_method_call(remote, method, "ss", (app_id, reason))
        # NB: jeepney does NOT raise on error replies -- the reply comes
        # back with message_type == error, so inspect it.
        try:
            reply = conn.send_and_get_reply(msg)
            if reply.header.message_type == MessageType.error:
                results.append(f"ERR {reply.body}")
            else:
                # reply.body is (cookie,) on success
                results.append(f"OK {reply.body[0]}")
        except Exception as e:  # transport-level failure
            results.append(f"ERR {e}")

    with open(outfile, "w") as f:
        f.write("\n".join(results) + "\n")

    # Stay on the bus (holding the inhibitors) until killed.
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
