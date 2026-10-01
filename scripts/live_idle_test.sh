#!/usr/bin/env bash
# Live integration tests for lion-idle 0.2.0.
#
# Brings up, all private to this run:
#   - a session bus + a fake system bus (dbus-daemon)
#   - examples/mock-peers (mock lion-locker / lion-power / lion-config /
#     org.freedesktop.login1, with a shared call log)
#   - scripts/mock_compositor.py (a real ext-idle-notify-v1 wire server,
#     FIFO-driven)
#   - scripts/inhibit_holder.py (jeepney) for *held* inhibitors -- a
#     plain gdbus call exits immediately, and the daemon's peer-death
#     cleanup (correctly) auto-releases such inhibitors
#   - the lion-idle daemon under test
#
# and then drives: threshold dispatch end to end (Wayland event ->
# D-Bus call ordering), inhibit on both surfaces, peer-death cleanup,
# live config reload, logind sleep/wake, limits, exit semantics.
#
# Self-locating: run from anywhere.

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
RUN="$ROOT/testrun"

BIN="$ROOT/target/release/lion-idle"
MOCK="$ROOT/target/release/examples/mock-peers"
COMPOSITOR="$HERE/mock_compositor.py"
HOLDER="$HERE/inhibit_holder.py"

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL $1${2:+ -- $2}"; }

export DBUS_SESSION_BUS_ADDRESS=""
export DBUS_SYSTEM_BUS_ADDRESS=""

HOLDERS=()

cleanup() {
    local code=$?
    [[ -n "${DAEMON_PID:-}" ]] && kill "$DAEMON_PID" 2>/dev/null
    [[ -n "${MOCK_PID:-}" ]] && kill "$MOCK_PID" 2>/dev/null
    [[ -n "${COMP_PID:-}" ]] && kill "$COMP_PID" 2>/dev/null
    [[ -n "${MON_PID:-}" ]] && kill "$MON_PID" 2>/dev/null
    for h in "${HOLDERS[@]:-}"; do [[ -n "$h" ]] && kill "$h" 2>/dev/null; done
    for b in "${BUS_PIDS[@]:-}"; do [[ -n "$b" ]] && kill "$b" 2>/dev/null; done
    sleep 0.3
    exit $code
}
trap cleanup EXIT INT TERM

G() { gdbus call --address "unix:path=$RUN/bus" --dest "$1" --object-path "$2" --method "$3" "${@:4}"; }
IDLE_CALL() { G "os.lionos.Idle" "/os/lionos/Idle" "os.lionos.Idle1.$1" "${@:2}"; }
SAVER_CALL() { G "org.freedesktop.ScreenSaver" "/org/freedesktop/ScreenSaver" "org.freedesktop.ScreenSaver.$1" "${@:2}"; }
MOCK_LOG() { G "com.mockpeers.Log" "/mock" "com.mockpeers.Log1.$1" "${@:2}"; }
CFG() { G "os.lionos.Config" "/os/lionos/Config" "os.lionos.Config1.$1" "${@:2}"; }
wl() { echo "$1" > "$RUN/compositor.fifo"; }

# Extract the returned cookie from gdbus output like `(uint32 7,)'.
# (A naive `grep -o [0-9]*` grabs the 32 from the *type name*.)
cookie_of() { sed -n 's/.*uint32 \([0-9]*\).*/\1/p' <<<"$1"; }

# The mock's GetLog comes back as one gdbus line; split into real lines.
mocklog_lines() {
    MOCK_LOG GetLog 2>/dev/null | sed "s/^(//; s/)$//; s/^'//; s/'$//" | sed "s/', *'/\\n/g"
}

wait_for() { # wait_for <desc> <seconds> <cmd...>
    local desc=$1 secs=$2; shift 2
    for _ in $(seq 1 $((secs * 10))); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.1
    done
    echo "timeout waiting for: $desc" >&2
    return 1
}

daemon_log() { grep -q "$1" "$RUN/daemon.log" 2>/dev/null; }
comp_log() { grep -q "$1" "$RUN/compositor.log" 2>/dev/null; }
comp_log_count() { grep -c "$1" "$RUN/compositor.log" 2>/dev/null || true; }
metrics() { IDLE_CALL GetMetrics 2>/dev/null; }
state() { IDLE_CALL GetState 2>/dev/null; }
thresholds() { IDLE_CALL GetThresholds 2>/dev/null; }

# Start a held inhibitor; echoes the cookie, registers the holder PID.
hold() { # hold <surface> <app> <reason>
    local surface=$1 app=$2 reason=$3
    local out="$RUN/holder_$$.txt"
    rm -f "$out"
    # NB: stdout/stderr MUST be redirected -- the caller runs inside a
    # command substitution, which otherwise waits forever for the
    # holder's inherited pipe to close.
    python3 "$HOLDER" "unix:path=$RUN/bus" "$surface" "$app" "$reason" "$out" >/dev/null 2>&1 &
    local pid=$!
    HOLDERS+=("$pid")
    for _ in $(seq 1 60); do
        [[ -s "$out" ]] && break
        sleep 0.1
    done
    head -1 "$out" | sed -n 's/^OK \([0-9]*\)/\1/p'
}

echo "== lion-idle 0.2.0 live integration tests =="
echo "== build =="
( cd "$ROOT" && export PATH="$HOME/.cargo/bin:$PATH" && \
  cargo build --release 2>/dev/null && \
  cargo build --release --example mock-peers 2>/dev/null ) || { echo "build failed"; exit 1; }

# ---------- environment ----------
rm -rf "$RUN"; mkdir -p "$RUN"
BUS_PIDS=()

SADDR=$(dbus-daemon --session --print-address=1 --address="unix:path=$RUN/bus" --fork 2>/dev/null | tail -1)
YADDR=$(dbus-daemon --session --print-address=1 --address="unix:path=$RUN/sysbus" --fork 2>/dev/null | tail -1)
export DBUS_SESSION_BUS_ADDRESS="$SADDR"
export DBUS_SYSTEM_BUS_ADDRESS="$YADDR"

"$MOCK" > "$RUN/mockpeers.log" 2>&1 &
MOCK_PID=$!

python3 "$COMPOSITOR" "$RUN/wayland.sock" "$RUN/compositor.fifo" "$RUN/compositor.log" &
COMP_PID=$!

wait_for "mock-peers" 15 gdbus call --address "$SADDR" --dest com.mockpeers.Log --object-path /mock --method com.mockpeers.Log1.GetLog || { echo "mock-peers never came up"; cat "$RUN/mockpeers.log"; exit 1; }

# ---------- start the daemon ----------
WAYLAND_DISPLAY="$RUN/wayland.sock" RUST_LOG=debug "$BIN" > "$RUN/daemon.log" 2>&1 &
DAEMON_PID=$!

wait_for "daemon bus name" 20 IDLE_CALL GetCapabilities || { echo "daemon never came up"; tail -40 "$RUN/daemon.log"; exit 1; }

dbus-monitor --address "unix:path=$RUN/bus" "type='signal',sender='os.lionos.Idle'" > "$RUN/signals.log" 2>/dev/null &
MON_PID=$!

echo
echo "== phase 1: startup surfaces =="
IDLE_CALL GetCapabilities | grep -q "screensaver-compat" && ok "T01a capabilities: screensaver-compat" || bad "T01a"
IDLE_CALL GetCapabilities | grep -q "catchup-rearm" && ok "T01b capabilities: catchup-rearm" || bad "T01b"
IDLE_CALL GetCapabilities | grep -q "suspend-implies-lock" && ok "T01c capabilities: suspend-implies-lock" || bad "T01c"
IDLE_CALL GetCapabilities | grep -q "logind-sleep" && ok "T01d capabilities: logind-sleep (fake system bus)" || bad "T01d"
IDLE_CALL GetCapabilities | grep -q "version:0.2.0" && ok "T01e capabilities: version" || bad "T01e"

gdbus call --address "$SADDR" --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner "os.lionos.Idle" | grep -q true && ok "T02 owns os.lionos.Idle" || bad "T02"
gdbus call --address "$SADDR" --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner "org.freedesktop.ScreenSaver" | grep -q true && ok "T03 owns org.freedesktop.ScreenSaver" || bad "T03"

TH=$(thresholds)
grep -q "dpms-ms" <<<"$TH" && grep -q "120000" <<<"$TH" && ok "T04a thresholds: dpms-ms 120000" || bad "T04a" "$TH"
grep -q "lock-ms" <<<"$TH" && grep -q "300000" <<<"$TH" && ok "T04b thresholds: lock-ms 300000" || bad "T04b" "$TH"
grep -q "suspend-ms" <<<"$TH" && grep -q "1800000" <<<"$TH" && ok "T04c thresholds: suspend-ms 1800000" || bad "T04c" "$TH"
grep -q "dim-ms" <<<"$TH" && bad "T04d dim should be absent (disabled)" || ok "T04d dim absent"

ST=$(state)
grep -q '"inhibited":false' <<<"$ST" && grep -q '"version":"0.2.0"' <<<"$ST" && ok "T05 GetState shape" || bad "T05" "$ST"

M=$(metrics)
grep -q "idled_fired_total" <<<"$M" && grep -q "catchup_rearms_total" <<<"$M" && ok "T06 metrics schema" || bad "T06"

sleep 0.5
comp_log "notification id=.* timeout=120000ms" && comp_log "notification id=.* timeout=300000ms" && comp_log "notification id=.* timeout=1800000ms" && ok "T07 compositor: all three notifications armed" || bad "T07" "$(tail -5 "$RUN/compositor.log")"
MOCK_LOG GetLog | grep -q "login1.Inhibit(sleep,delay)" && ok "T08 logind delay inhibitor taken" || bad "T08"

# ---------- phase 2: threshold events end to end ----------
echo
echo "== phase 2: dispatch end to end (Wayland -> D-Bus) =="
IDLE_CALL ResetMetrics >/dev/null; MOCK_LOG ClearLog >/dev/null

wl "IDLE 120000"; sleep 0.6
mocklog_lines | grep -q "power.DpmsOff" && ok "T09 dpms threshold -> power.DpmsOff" || bad "T09" "$(mocklog_lines | head -5)"

state | grep -q '"dpms_off":true' && SAVER_CALL GetActive | grep -q true && ok "T10 display state + ScreenSaver.GetActive" || bad "T10"

wl "RESUME 120000"; sleep 0.6
mocklog_lines | grep -q "power.DpmsOn" && ok "T11 resume -> power.DpmsOn" || bad "T11"

wl "IDLE 300000"; sleep 0.6
mocklog_lines | grep -q "locker.Lock" && ok "T12 lock threshold -> locker.Lock" || bad "T12"

wl "RESUME 300000"; sleep 0.4
NLOCK=$(mocklog_lines | grep -c "locker.Lock")
[[ "$NLOCK" == "1" ]] && ok "T13 lock resume: no unlock call (locker.Lock count still 1)" || bad "T13" "count=$NLOCK"

MOCK_LOG ClearLog >/dev/null
wl "IDLE 1800000"; sleep 0.8
LOG=$(mocklog_lines)
LOCKPOS=$(grep -n "locker.Lock" <<<"$LOG" | head -1 | cut -d: -f1)
SUSPPOS=$(grep -n "power.Suspend" <<<"$LOG" | head -1 | cut -d: -f1)
if [[ -n "$LOCKPOS" && -n "$SUSPPOS" && "$LOCKPOS" -lt "$SUSPPOS" ]]; then
    ok "T14 suspend -> locker.Lock BEFORE power.Suspend"
else
    bad "T14" "lock=$LOCKPOS suspend=$SUSPPOS log=$(echo "$LOG" | tr '\n' '|')"
fi
M=$(metrics)
grep -q '"idled_fired_total":3' <<<"$M" && grep -q '"idled_dpms":1' <<<"$M" && grep -q '"idled_lock":1' <<<"$M" && grep -q '"idled_suspend":1' <<<"$M" && ok "T15 metrics: one fire per threshold" || bad "T15" "$M"

# ---------- phase 3: inhibit + suppression + rearm ----------
echo
echo "== phase 3: inhibit, suppression, catch-up re-arm =="
IDLE_CALL ResetMetrics >/dev/null
MOCK_LOG ClearLog >/dev/null

COOKIE=$(hold "native" "native-app" "video playing")
[[ -n "$COOKIE" && "$COOKIE" -ge 1 ]] && ok "T16 native Inhibit -> held cookie $COOKIE" || bad "T16"
state | grep -q '"inhibited":true' && ok "T17 GetState inhibited" || bad "T17" "$(state)"

wl "IDLE 120000"; sleep 0.6
mocklog_lines | grep -q "power.DpmsOff" && bad "T18 action must be suppressed while inhibited" || ok "T18 dpms suppressed while inhibited"
M=$(metrics)
grep -q '"actions_suppressed_total":1' <<<"$M" && ok "T19 suppression counted" || bad "T19" "$M"

IDLE_CALL Uninhibit "$COOKIE" >/dev/null; sleep 0.8
M=$(metrics)
grep -q '"catchup_rearms_total":1' <<<"$M" && ok "T20 last release -> catch-up re-arm (metrics)" || bad "T20" "$M"
ARMS_AFTER=$(comp_log_count "timeout=120000ms")
[[ "$ARMS_AFTER" -ge 3 ]] && ok "T20b compositor saw the re-armed notification (120000 x$ARMS_AFTER)" || bad "T20b" "arms=$ARMS_AFTER"

grep -q "InhibitedChanged" "$RUN/signals.log" && ok "T21 InhibitedChanged signal emitted" || bad "T21"

# ---------- phase 4: org.freedesktop.ScreenSaver compatibility ----------
echo
echo "== phase 4: org.freedesktop.ScreenSaver compat =="
wl "RESUME 120000"; sleep 0.3
MOCK_LOG ClearLog >/dev/null

SC=$(hold "screensaver" "firefox" "watching video")
[[ -n "$SC" && "$SC" -ge 1 ]] && ok "T22 compat Inhibit -> held cookie $SC" || bad "T22"
state | grep -q '"inhibited":true' && ok "T22b shared registry: state inhibited" || bad "T22b" "$(state)"

wl "IDLE 300000"; sleep 0.5
mocklog_lines | grep -q "locker.Lock" && bad "T22c lock must stay suppressed under compat inhibit" || ok "T22c lock suppressed under compat inhibit"
M=$(metrics)
grep -q '"actions_suppressed_total":2' <<<"$M" && ok "T22d second suppression counted" || bad "T22d" "$M"

SAVER_CALL UnInhibit "$SC" >/dev/null; sleep 0.8
state | grep -q '"inhibited":false' && ok "T23 compat UnInhibit releases + re-arms (registry shared)" || bad "T23" "$(state)"
M=$(metrics)
grep -q '"catchup_rearms_total":2' <<<"$M" && ok "T23b second re-arm counted" || bad "T23b" "$M"

LC=$(cookie_of "$(SAVER_CALL Inhibit "legacy-app" "kde-style path" 2>/dev/null)")
sleep 0.5  # let its (one-shot) peer die on purpose: legacy path call works, then auto-release
[[ -n "$LC" && "$LC" -ge 1 ]] && ok "T24 legacy path /ScreenSaver serves Inhibit" || bad "T24" "$LC"
SAVER_LEGACY_RESP=$(gdbus call --address "unix:path=$RUN/bus" --dest "org.freedesktop.ScreenSaver" --object-path "/ScreenSaver" --method "org.freedesktop.ScreenSaver.GetActive" 2>/dev/null)
grep -q "false" <<<"$SAVER_LEGACY_RESP" && ok "T24b legacy path answers GetActive after auto-release" || bad "T24b" "$SAVER_LEGACY_RESP"

MOCK_LOG ClearLog >/dev/null
RES=$(SAVER_CALL SetActive true)
sleep 0.5
mocklog_lines | grep -q "locker.Lock" && ok "T25 SetActive(true) -> locker.Lock" || bad "T25" "$(mocklog_lines | head -3)"
grep -q "true" <<<"$RES" && ok "T25b SetActive(true) returns true" || bad "T25b" "$RES"

RESF=$(SAVER_CALL SetActive false)
grep -q "false" <<<"$RESF" && ok "T26 SetActive(false) refused (no un-lock)" || bad "T26" "$RESF"

# ---------- phase 5: peer-death cleanup ----------
echo
echo "== phase 5: peer-death cleanup =="
BEFORE=$(metrics | grep -o '"inhibit_released_peer_total":[0-9]*' | grep -o '[0-9]*$')
PC=$(cookie_of "$(gdbus call --address "unix:path=$RUN/bus" --dest "os.lionos.Idle" --object-path "/os/lionos/Idle" --method "os.lionos.Idle1.Inhibit" "doomed-app" "crashing soon" 2>/dev/null)")
sleep 1.0
state | grep -q '"inhibited":false' && ok "T27 dead peer's inhibitor auto-released (cookie $PC)" || bad "T27" "$(state)"
AFTER=$(metrics | grep -o '"inhibit_released_peer_total":[0-9]*' | grep -o '[0-9]*$')
[[ -n "$AFTER" && -n "$BEFORE" && "$AFTER" -gt "$BEFORE" ]] && ok "T28 peer-release counted ($BEFORE -> $AFTER)" || bad "T28" "before=$BEFORE after=$AFTER"
daemon_log "idle inhibit released (peer disconnected)" && ok "T28b release logged with owner identity" || bad "T28b"

# ---------- phase 6: live config reload ----------
echo
echo "== phase 6: live config reload =="
IDLE_CALL ResetMetrics >/dev/null

CFG Set "org.lionos.idle" "lock-ms" 400000 >/dev/null; sleep 1.0
thresholds | grep -q "lock-ms" && thresholds | grep -q "400000" && ok "T29 lock-ms reloaded live" || bad "T29" "$(thresholds)"
comp_log "notification id=.* timeout=400000ms" && ok "T30 compositor re-armed at 400000" || bad "T30" "$(tail -3 "$RUN/compositor.log")"
grep -q "ThresholdsChanged" "$RUN/signals.log" && ok "T31 ThresholdsChanged signal" || bad "T31"

CFG Set "org.lionos.idle" "dim-ms" 60000 >/dev/null; sleep 1.0
thresholds | grep -q "60000" && comp_log "notification id=.* timeout=60000ms" && ok "T32 dim enabled live + armed" || bad "T32" "$(thresholds)"

B400=$(comp_log_count "timeout=400000ms")
CFG Set "org.lionos.idle" "lock-ms" 0 >/dev/null; sleep 1.0
grep -q "lock-ms" <<<"$(thresholds)" && bad "T33 lock-ms=0 should read disabled" || ok "T33 lock disabled by 0"
A400=$(comp_log_count "timeout=400000ms")
[[ "$A400" == "$B400" ]] && ok "T33b lock notification not recreated (still $A400 creations)" || bad "T33b" "before=$B400 after=$A400"

M=$(metrics)
grep -q '"config_reloads_total":3' <<<"$M" && ok "T34 three reloads counted" || bad "T34" "$M"

# ---------- phase 7: logind sleep integration ----------
echo
echo "== phase 7: logind lock-before-sleep + wake restore =="
IDLE_CALL ResetMetrics >/dev/null; MOCK_LOG ClearLog >/dev/null

MOCK_LOG EmitPrepareForSleep true >/dev/null; sleep 1.0
MOCK_LOG EmitPrepareForSleep false >/dev/null; sleep 1.0

LOG=$(mocklog_lines)
grep -q "locker.Lock" <<<"$LOG" && ok "T35a PrepareForSleep(true) -> locker.Lock" || bad "T35a" "$LOG"
grep -q "power.Undim" <<<"$LOG" && grep -q "power.DpmsOn" <<<"$LOG" && ok "T35b wake -> Undim + DpmsOn" || bad "T35b" "$LOG"
DIMARMS=$(comp_log_count "timeout=60000ms")
[[ "$DIMARMS" -ge 2 ]] && ok "T35c wake -> idle clocks re-armed (dim arms=$DIMARMS)" || bad "T35c" "dim arms=$DIMARMS"

M=$(metrics)
grep -q '"logind_sleep_total":1' <<<"$M" && grep -q '"logind_wake_total":1' <<<"$M" && ok "T36 sleep/wake metrics" || bad "T36" "$M"
INH=$(mocklog_lines | grep -c "login1.Inhibit(sleep,delay)")
[[ "$INH" -eq 1 ]] && ok "T37 delay inhibitor re-taken once after wake" || bad "T37" "inhibit calls=$INH"

MOCK_LOG ClearLog >/dev/null
wl "IDLE 60000"; sleep 0.5
mocklog_lines | grep -q "power.Dim" && ok "T37b live-loaded dim threshold dispatches (IDLE 60000 -> power.Dim)" || bad "T37b" "$(mocklog_lines | head -3)"

# ---------- phase 8: inhibit limits ----------
echo
echo "== phase 8: inhibit limits =="
IDLE_CALL ResetMetrics >/dev/null
LIMIT_OUT="$RUN/limit_out.txt"
rm -f "$LIMIT_OUT"
# One stay-alive peer takes 129 inhibitors; the 129th must be rejected.
python3 "$HOLDER" "unix:path=$RUN/bus" "native" "spam" "filling the registry" "$LIMIT_OUT" 129 >/dev/null 2>&1 &
LIMIT_PID=$!
HOLDERS+=("$LIMIT_PID")
for _ in $(seq 1 100); do
    [[ -s "$LIMIT_OUT" ]] && break
    sleep 0.1
done
sleep 0.5
OKN=$(grep -c "^OK" "$LIMIT_OUT" 2>/dev/null || true)
ERRN=$(grep -c "^ERR" "$LIMIT_OUT" 2>/dev/null || true)
[[ "$OKN" == "128" && "$ERRN" == "1" ]] && ok "T38 limit enforced: 128 granted, 129th LimitsExceeded" || bad "T38" "ok=$OKN err=$ERRN"
M=$(metrics)
grep -q '"inhibit_rejected_total":1' <<<"$M" && ok "T39 rejection counted" || bad "T39" "$M"
kill "$LIMIT_PID" 2>/dev/null
sleep 0.8
state | grep -q '"inhibited":false' && ok "T39b registry drained by peer death (all 128 released)" || bad "T39b" "$(state)"

# ---------- phase 9: exit semantics + CLI ----------
echo
echo "== phase 9: exit semantics + CLI =="
WLCODE=0
wl "QUIT"
for _ in $(seq 1 50); do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        wait "$DAEMON_PID"; WLCODE=$?
        break
    fi
    sleep 0.1
done
if kill -0 "$DAEMON_PID" 2>/dev/null; then
    bad "T40 daemon should exit on compositor loss"
else
    [[ "$WLCODE" -ne 0 ]] && ok "T40 Wayland loss -> exit $WLCODE (non-zero: Restart=on-failure applies)" || bad "T40" "exit=$WLCODE"
fi
daemon_log "Wayland event stream ended" && ok "T40b reason logged" || bad "T40b"

VER=$("$BIN" --version)
grep -q "0.2.0" <<<"$VER" && ok "T41 --version" || bad "T41" "$VER"

PC_OUT=$(DBUS_SESSION_BUS_ADDRESS="$SADDR" "$BIN" --print-config 2>&1)
grep -q "lion-config (live)" <<<"$PC_OUT" && grep -q "120000" <<<"$PC_OUT" && ok "T42 --print-config live values" || bad "T42" "$PC_OUT"

PC_DEF=$(env -u DBUS_SESSION_BUS_ADDRESS "$BIN" --print-config 2>&1)
grep -q "hardcoded defaults" <<<"$PC_DEF" && grep -q "300000" <<<"$PC_DEF" && ok "T43 --print-config fallback defaults" || bad "T43" "$PC_DEF"

# ---------- summary ----------
echo
echo "== summary: $PASS passed, $FAIL failed =="
[[ "$FAIL" -eq 0 ]] && echo "ALL GREEN" || echo "FAILURES PRESENT"
exit $((FAIL > 0))
