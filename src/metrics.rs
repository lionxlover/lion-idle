//! Atomic counters exposed via `GetMetrics()` (same contract shape as
//! lion-greeter and lion-locker). Lock-free: every counter is a relaxed
//! atomic -- these are telemetry, not control flow.
//!
//! The JSON is hand-rolled rather than pulled in via serde_json: the
//! schema is a flat map of fixed snake_case keys to integers, so no
//! escaping is ever needed, and one fewer dependency keeps the "small on
//! purpose" promise honest.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Instant;

static IDLED_TOTAL: AtomicU64 = AtomicU64::new(0);
static IDLED_DIM: AtomicU64 = AtomicU64::new(0);
static IDLED_DPMS: AtomicU64 = AtomicU64::new(0);
static IDLED_LOCK: AtomicU64 = AtomicU64::new(0);
static IDLED_SUSPEND: AtomicU64 = AtomicU64::new(0);
static RESUMED_TOTAL: AtomicU64 = AtomicU64::new(0);
static ACTIONS_SUPPRESSED: AtomicU64 = AtomicU64::new(0);
static ACTION_FAILURES: AtomicU64 = AtomicU64::new(0);
static INHIBIT_GRANTED: AtomicU64 = AtomicU64::new(0);
static INHIBIT_GRANTED_SCREENSAVER: AtomicU64 = AtomicU64::new(0);
static INHIBIT_REJECTED: AtomicU64 = AtomicU64::new(0);
static INHIBIT_RELEASED: AtomicU64 = AtomicU64::new(0);
static INHIBIT_RELEASED_PEER: AtomicU64 = AtomicU64::new(0);
static CONFIG_RELOADS: AtomicU64 = AtomicU64::new(0);
static CATCHUP_REARMS: AtomicU64 = AtomicU64::new(0);
static SUSPEND_LOCKS: AtomicU64 = AtomicU64::new(0);
static LOGIND_SLEEPS: AtomicU64 = AtomicU64::new(0);
static LOGIND_WAKES: AtomicU64 = AtomicU64::new(0);
static SCREENSAVER_ACTIVATE: AtomicU64 = AtomicU64::new(0);

static STARTED_UNIX: AtomicU64 = AtomicU64::new(0);
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn init_started_unix() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    STARTED_UNIX.store(now, Ordering::Relaxed);
}

pub fn inc_idled(which: &str) {
    IDLED_TOTAL.fetch_add(1, Ordering::Relaxed);
    match which {
        "dim" => IDLED_DIM.fetch_add(1, Ordering::Relaxed),
        "dpms" => IDLED_DPMS.fetch_add(1, Ordering::Relaxed),
        "lock" => IDLED_LOCK.fetch_add(1, Ordering::Relaxed),
        "suspend" => IDLED_SUSPEND.fetch_add(1, Ordering::Relaxed),
        _ => 0,
    };
}

pub fn inc_resumed() {
    RESUMED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_suppressed() {
    ACTIONS_SUPPRESSED.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_action_failure() {
    ACTION_FAILURES.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_inhibit_granted(screensaver: bool) {
    INHIBIT_GRANTED.fetch_add(1, Ordering::Relaxed);
    if screensaver {
        INHIBIT_GRANTED_SCREENSAVER.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn inc_inhibit_rejected() {
    INHIBIT_REJECTED.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_inhibit_released(peer_gone: bool) {
    INHIBIT_RELEASED.fetch_add(1, Ordering::Relaxed);
    if peer_gone {
        INHIBIT_RELEASED_PEER.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn inc_config_reload() {
    CONFIG_RELOADS.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_catchup_rearm() {
    CATCHUP_REARMS.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_suspend_lock() {
    SUSPEND_LOCKS.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_logind_sleep() {
    LOGIND_SLEEPS.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_logind_wake() {
    LOGIND_WAKES.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_screensaver_activate() {
    SCREENSAVER_ACTIVATE.fetch_add(1, Ordering::Relaxed);
}

/// Snapshot as JSON (schema-stable; fields only ever get added).
pub fn to_json() -> String {
    fn v(a: &AtomicU64) -> u64 {
        a.load(Ordering::Relaxed)
    }
    format!(
        concat!(
            "{{",
            "\"idled_fired_total\":{},",
            "\"idled_dim\":{},\"idled_dpms\":{},\"idled_lock\":{},\"idled_suspend\":{},",
            "\"resumed_total\":{},",
            "\"actions_suppressed_total\":{},\"action_failures_total\":{},",
            "\"inhibit_granted_total\":{},\"inhibit_granted_screensaver\":{},",
            "\"inhibit_rejected_total\":{},\"inhibit_released_total\":{},",
            "\"inhibit_released_peer_total\":{},",
            "\"config_reloads_total\":{},\"catchup_rearms_total\":{},",
            "\"suspend_locks_total\":{},",
            "\"logind_sleep_total\":{},\"logind_wake_total\":{},",
            "\"screensaver_activate_total\":{},",
            "\"started_unix\":{},\"uptime_ms\":{}",
            "}}"
        ),
        v(&IDLED_TOTAL),
        v(&IDLED_DIM),
        v(&IDLED_DPMS),
        v(&IDLED_LOCK),
        v(&IDLED_SUSPEND),
        v(&RESUMED_TOTAL),
        v(&ACTIONS_SUPPRESSED),
        v(&ACTION_FAILURES),
        v(&INHIBIT_GRANTED),
        v(&INHIBIT_GRANTED_SCREENSAVER),
        v(&INHIBIT_REJECTED),
        v(&INHIBIT_RELEASED),
        v(&INHIBIT_RELEASED_PEER),
        v(&CONFIG_RELOADS),
        v(&CATCHUP_REARMS),
        v(&SUSPEND_LOCKS),
        v(&LOGIND_SLEEPS),
        v(&LOGIND_WAKES),
        v(&SCREENSAVER_ACTIVATE),
        v(&STARTED_UNIX),
        STARTED.elapsed().as_millis(),
    )
}

/// Reset every counter (test and support tooling; `ResetMetrics()`).
pub fn reset() {
    for a in [
        &IDLED_TOTAL,
        &IDLED_DIM,
        &IDLED_DPMS,
        &IDLED_LOCK,
        &IDLED_SUSPEND,
        &RESUMED_TOTAL,
        &ACTIONS_SUPPRESSED,
        &ACTION_FAILURES,
        &INHIBIT_GRANTED,
        &INHIBIT_GRANTED_SCREENSAVER,
        &INHIBIT_REJECTED,
        &INHIBIT_RELEASED,
        &INHIBIT_RELEASED_PEER,
        &CONFIG_RELOADS,
        &CATCHUP_REARMS,
        &SUSPEND_LOCKS,
        &LOGIND_SLEEPS,
        &LOGIND_WAKES,
        &SCREENSAVER_ACTIVATE,
    ] {
        a.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_has_stable_shape() {
        let j = to_json();
        for key in [
            "idled_fired_total",
            "idled_suspend",
            "resumed_total",
            "inhibit_granted_screensaver",
            "catchup_rearms_total",
            "suspend_locks_total",
            "logind_sleep_total",
            "uptime_ms",
        ] {
            assert!(j.contains(&format!("\"{key}\":")), "missing {key} in {j}");
        }
        // flat integers only -- no strings to escape
        assert!(!j.contains('\\'));
        assert!(j.starts_with('{') && j.ends_with('}'));
    }

    #[test]
    fn counters_count_and_reset() {
        reset();
        inc_idled("suspend");
        inc_idled("dim");
        inc_suppressed();
        inc_inhibit_granted(true);
        inc_inhibit_released(true);
        inc_logind_wake();
        let j = to_json();
        assert!(j.contains("\"idled_fired_total\":2"));
        assert!(j.contains("\"idled_dim\":1"));
        assert!(j.contains("\"idled_suspend\":1"));
        assert!(j.contains("\"inhibit_granted_screensaver\":1"));
        assert!(j.contains("\"inhibit_released_peer_total\":1"));
        assert!(j.contains("\"logind_wake_total\":1"));
        reset();
        assert!(to_json().contains("\"idled_fired_total\":0"));
    }
}
