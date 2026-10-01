# lion-idle

LionOS idle-watching daemon. Small on purpose: it decides *when*, and asks
`lion-locker` / `lion-power` to actually do the thing.

## What it does
- One `ext-idle-notify-v1` notification per configured, non-zero
  threshold (dim, DPMS off, lock, suspend) -- the compositor tracks idle
  time and resets on activity; this daemon just reacts
- Reads thresholds from `lion-config`, live (re-reads on `Changed`), with
  safe hardcoded fallbacks if that service is unreachable; the watcher
  task survives `lion-config` restarting and resubscribes when it returns
- Fires the matching call: dim/DPMS/suspend -> `lion-power`, lock ->
  `lion-locker`; reverses dim/DPMS on `resumed`
- **Suspend implies lock**: the suspend threshold locks first, always,
  even when the lock threshold is disabled -- sessions do not suspend
  unlocked (macOS/Windows/GNOME parity; `lion-locker`'s `Lock()` is
  idempotent, so the safety net is free)
- **logind sleep integration** (gdm's pattern): a sleep delay inhibitor
  is held; on `PrepareForSleep(true)` the session is locked (bounded by
  3 s, under logind's default 5 s delay budget) and the inhibitor is
  released so the machine can go down; on wake the display is restored
  (`Undim` + `DpmsOn`) and the idle clocks re-armed. Covers *every*
  sleep path -- `systemctl suspend`, lid close, another client of
  `lion-power` -- not just this daemon's own threshold
- An `Inhibit`/`Uninhibit` surface so an app (video call, playback) can
  suppress actions temporarily; auto-released if that app disappears
  from the bus without calling `Uninhibit`
- **`org.freedesktop.ScreenSaver` compatibility** at both
  `/org/freedesktop/ScreenSaver` and `/ScreenSaver`: Chrome, Firefox,
  mpv, vlc and friends keep the screen awake the way they expect to;
  `SetActive(true)` locks the session, `SetActive(false)` is refused
  (no un-lock without authentication)
- **Catch-up re-arm**: when the last inhibitor goes away mid-idle, the
  Wayland notifications are recreated, which per the protocol restarts
  each timeout from that moment -- so "the video ended and the user
  walked away without touching anything" still locks, instead of
  waiting for input activity that may never come

## What it deliberately does not do
- Render or drive the lock UI -- entirely `lion-locker`'s job
- Implement suspend mechanics -- entirely `lion-power`'s job; this only
  calls `Suspend()`
- mlock: unlike the greeter and the locker, this daemon never holds a
  secret -- thresholds and inhibit reasons are not credentials -- so
  the cost of the extra syscall and the RLIMIT dance buys nothing here

## Interface
`os.lionos.Idle1` (session bus, `os.lionos.Idle` at `/os/lionos/Idle`):

| Member | Shape |
|---|---|
| `Inhibit(app_id, reason) -> cookie` | suppress actions until released (limit: 128 live) |
| `Uninhibit(cookie)` | release |
| `GetThresholds() -> a{sv}` | ms per threshold; absent key = disabled |
| `ThresholdsChanged(a{sv})` | signal, on every live reload |
| `InhibitedChanged(b, u)` | signal, on every grant/release |
| `GetState() -> s` | JSON snapshot |
| `GetMetrics() / ResetMetrics()` | JSON counters (schema-stable) |
| `GetCapabilities() -> as` | feature negotiation |

Plus the `org.freedesktop.ScreenSaver` surface above, sharing the same
cookie registry, limits, and peer-death cleanup.

## Exit codes (systemd-facing contract)
- `0`: only `--version` / `--print-config`. The daemon itself never
  exits 0: a Wayland connection loss returns a *non-zero* exit so
  `Restart=on-failure` brings the session's idle watching back.
- Startup retries transient compositor failures (login race) in-process
  with backoff; a compositor that exists but lacks
  `ext-idle-notify-v1` is permanent and fails fast.

## CLI
```
lion-idle --version        # 0.2.0
lion-idle --print-config   # resolved thresholds (live or defaults)
lion-idle --help
```

## Assumed contracts
`lion-config`'s and `lion-power`'s real D-Bus surfaces weren't available
when this was built; the minimal contract assumed for each is documented
in `src/config.rs` and `src/dispatch.rs` respectively (and
`etc/lionos/idle-schema.md` for the config schema specifically).

## Verification
- `cargo test` -- 34 unit tests (dispatch policy table, inhibit
  limits/truncation/peer cleanup, config parsing incl. wrong-type
  handling, capabilities, wake-pipe semantics, display state)
- `scripts/live_idle_test.sh` -- 64 end-to-end checks against a private
  bus stack: a **mock Wayland compositor** (real `ext-idle-notify-v1`
  wire protocol, FIFO-driven), mock locker/power/config/login1 peers
  (`examples/mock-peers.rs`), and held-inhibitor callers
  (`scripts/inhibit_holder.py`). Covers dispatch ordering
  (lock-before-suspend), suppression, catch-up re-arm, live config
  reload, logind sleep/wake, the 128-inhibit limit, peer-death cleanup,
  and the exit-code contract.

## Files
- `src/wayland.rs`       -- the ext-idle-notify-v1 client (poll(2) loop,
                          wake pipe, re-arm, retry classification)
- `src/config.rs`        -- lion-config client, live thresholds
- `src/dispatch.rs`      -- the action policy table + Locker/Power proxies
- `src/inhibit.rs`       -- inhibitor registry (bounded, truncated)
- `src/screensaver.rs`   -- org.freedesktop.ScreenSaver compatibility
- `src/logind.rs`        -- PrepareForSleep lock-before-sleep + wake restore
- `src/peers.rs`         -- releases inhibitors when their owner disconnects
- `src/metrics.rs`       -- lock-free counters
- `src/service.rs`       -- wiring + the public os.lionos.Idle service
