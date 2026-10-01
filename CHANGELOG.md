# Changelog

## 0.2.0

Fixed (0.1.0 shipped having never compiled; both errors were in
`src/wayland.rs`):

- `registry_queue_init` requires a `Dispatch<WlRegistry,
  GlobalListContents>` impl -- provided (no-op; no hotplug needs).
- `Dispatch<ExtIdleNotifierV1, ()>` had the wrong `event` signature.

Behavioral fixes:

- **Wayland connection loss now exits non-zero.** 0.1.0's thread
  returned quietly, `serve()` returned `Ok(())`, the process exited 0,
  and `Restart=on-failure` correctly read that as "nothing to do" --
  leaving the session with a dead idle daemon until a human noticed.
- **Reconfigures are no longer latent.** 0.1.0 drained the command
  channel only between `blocking_dispatch` calls, which block until the
  next Wayland event -- exactly the thing that stops arriving once the
  user is idle. A config change could sit unprocessed for minutes. The
  thread now `poll(2)`s the Wayland socket and a wake pipe together.
- **Startup survives the login race.** Compositor-not-yet-up is retried
  with backoff in-process; compositor-lacks-protocol fails fast.
  (0.1.0 also called `oneshot::blocking_recv()` from async context --
  a documented tokio panic -- now a plain `.await`.)
- **Wrong-type config values are no longer silent.** A string in
  `lock-ms` still reads as "disabled", but now warns loudly.

Security:

- **Suspend implies lock.** The suspend threshold calls `Lock()` before
  `Suspend()` unconditionally, so a machine that suspends never wakes
  unlocked even when the lock threshold is disabled.
- **logind `PrepareForSleep` integration** (gdm's pattern, with the
  delay-inhibitor budget respected): lock before *any* sleep
  (`systemctl suspend`, lid close, other clients), restore the display
  and restart idle clocks on wake.
- **Inhibit is bounded**: at most 128 live cookies, caller strings
  truncated at 256 bytes (0.1.0 allowed unbounded registry growth by
  any session app). Overflow returns
  `org.freedesktop.DBus.Error.LimitsExceeded`.
- `org.freedesktop.ScreenSaver.SetActive(false)` is refused: there is
  no un-lock without authentication.

Features:

- **`org.freedesktop.ScreenSaver` compatibility** (`Inhibit` /
  `UnInhibit` / `GetActive` / `SetActive`) at both
  `/org/freedesktop/ScreenSaver` and `/ScreenSaver`, sharing the native
  surface's cookie registry and peer-death cleanup. Chrome, Firefox,
  mpv and friends can now keep the screen awake.
- **Catch-up re-arm** on the last inhibitor's release: Wayland
  notifications are recreated, restarting each timeout from that moment
  (per the protocol) -- "video ended, user walked away" still locks.
- `GetThresholds` / `ThresholdsChanged`, `GetState`, `GetMetrics` /
  `ResetMetrics`, `GetCapabilities` (mirrors the greeter/locker
  observability contract).
- `InhibitedChanged` signal on every grant/release, whatever surface
  the caller used.
- Config watcher survives `lion-config` restarts (resubscribes when the
  service returns).
- CLI: `--version`, `--print-config` (live values or defaults),
  `--help`.

Packaging / testing:

- systemd unit hardened (NoNewPrivileges, ProtectSystem=strict,
  AF_UNIX-only, no namespaces, ...; `TimeoutStartSec=90` for the
  compositor retry window).
- D-Bus policy updated for both owned names.
- 34 unit tests (0.1.0 had none) + 64-check live integration suite
  (`scripts/live_idle_test.sh`): a real-protocol mock Wayland
  compositor, mock locker/power/config/login1 peers, held-inhibitor
  callers; three consecutive all-green runs.
