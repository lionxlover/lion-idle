//! The actions this daemon is allowed to take. Everything about *how* to
//! lock, dim, switch off the display, or suspend belongs to
//! `lion-locker` / `lion-power`; this module only names the action for
//! each threshold event.
//!
//! `lion-power`'s exact D-Bus surface is likewise assumed and documented
//! here (its own build prompt covers the daemon, not this contract):
//!   Bus / path  : os.lionos.Power / /os/lionos/Power
//!   Interface   : os.lionos.Power1
//!   Dim() / Undim()           soften backlight ahead of DPMS off, and
//!                             restore it on resume
//!   DpmsOff() / DpmsOn()      display power state
//!   Suspend()                 already implied by lion-session's own
//!                             assumed contract (see its Cargo README)
//!
//! `os.lionos.Locker1`'s `Lock()` is the one already built in lion-locker.
//!
//! 0.2.0 policy changes, both deliberate:
//! - **Suspend implies lock.** When the suspend threshold fires, `Lock()`
//!   is called before `Suspend()` -- unconditionally, even when the lock
//!   threshold itself is disabled. A machine that suspends unlocked
//!   wakes unlocked; macOS, Windows and GNOME all close this hole by
//!   default, and `lion-locker` treats a redundant `Lock()` as a no-op,
//!   so the safety net is free.
//! - The decision table is a pure function (`action_for`) behind a
//!   narrow trait (`DispatchApi`), so the dispatch policy is unit-testable
//!   without a bus -- and the live integration tests can swap in a
//!   recording fake.

use anyhow::Result;
use std::time::Duration;
use zbus::{proxy, Connection};

use crate::wayland::Threshold;

/// Everything this daemon may ask a peer to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Dim,
    Undim,
    DpmsOff,
    DpmsOn,
    Lock,
    /// Lock, then suspend. One composite so the ordering lives in one
    /// place and the trait surface stays small.
    LockThenSuspend,
}

/// Which action (if any) a Wayland event maps to. `idle == true` means
/// the threshold was reached; `false` means activity resumed.
///
/// Reverse actions exist only for dim and DPMS: a lock is reversed by
/// entering the password (`lion-locker`'s job, not ours), and a
/// suspend's "reverse" is the machine waking up (handled by the logind
/// wake path, which restores the display).
pub fn action_for(threshold: Threshold, idle: bool) -> Option<Action> {
    match (threshold, idle) {
        (Threshold::Dim, true) => Some(Action::Dim),
        (Threshold::Dim, false) => Some(Action::Undim),
        (Threshold::Dpms, true) => Some(Action::DpmsOff),
        (Threshold::Dpms, false) => Some(Action::DpmsOn),
        (Threshold::Lock, true) => Some(Action::Lock),
        (Threshold::Lock, false) => None,
        (Threshold::Suspend, true) => Some(Action::LockThenSuspend),
        (Threshold::Suspend, false) => None,
    }
}

/// The narrow surface `service.rs` depends on, so the event loop is
/// testable against a recording fake. Static-dispatch only (async fn in
/// trait), which is all the daemon needs.
pub trait DispatchApi {
    /// Execute one action. Errors are the *peer* failing (locker/power
    /// down, method rejected); implementations decide per-action policy
    /// (see `Dispatcher::invoke` for the production behavior).
    async fn invoke(&self, action: Action) -> Result<()>;
}

#[proxy(
    interface = "os.lionos.Locker1",
    default_service = "os.lionos.Locker",
    default_path = "/os/lionos/Locker"
)]
pub(crate) trait Locker1 {
    async fn lock(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "os.lionos.Power1",
    default_service = "os.lionos.Power",
    default_path = "/os/lionos/Power"
)]
trait Power1 {
    async fn dim(&self) -> zbus::Result<()>;
    async fn undim(&self) -> zbus::Result<()>;
    async fn dpms_off(&self) -> zbus::Result<()>;
    async fn dpms_on(&self) -> zbus::Result<()>;
    async fn suspend(&self) -> zbus::Result<()>;
}

/// Deadline for the pre-suspend `Lock()` call: logind's default
/// `InhibitDelayMaxSec` is 5 s, and the logind path holds a delay
/// inhibitor for roughly this window, so the lock call must give up
/// before the delay budget does. A lock that can't complete in 3 s is
/// not going to complete at all; the suspend still proceeds (power
/// policy must not be hostage to a locker bug -- that trade is
/// deliberate and logged).
const LOCK_DEADLINE: Duration = Duration::from_secs(3);

pub struct Dispatcher {
    locker: Locker1Proxy<'static>,
    power: Power1Proxy<'static>,
}

impl Dispatcher {
    pub async fn new(conn: &Connection) -> Result<Self> {
        Ok(Self {
            locker: Locker1Proxy::new(conn).await?,
            power: Power1Proxy::new(conn).await?,
        })
    }
}

impl DispatchApi for Dispatcher {
    async fn invoke(&self, action: Action) -> Result<()> {
        match action {
            Action::Dim => self.power.dim().await.map_err(Into::into),
            Action::Undim => self.power.undim().await.map_err(Into::into),
            Action::DpmsOff => self.power.dpms_off().await.map_err(Into::into),
            Action::DpmsOn => self.power.dpms_on().await.map_err(Into::into),
            Action::Lock => self.locker.lock().await.map_err(Into::into),
            Action::LockThenSuspend => {
                // Safety net before the machine goes away (see module
                // docs). Suspend proceeds even if locking failed -- a
                // warn is left in the log either way.
                match tokio::time::timeout(LOCK_DEADLINE, self.locker.lock()).await {
                    Ok(Ok(())) => {
                        crate::metrics::inc_suspend_lock();
                        tracing::info!("session locked ahead of suspend");
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "pre-suspend Lock() failed; suspending anyway");
                        crate::metrics::inc_action_failure();
                    }
                    Err(_) => {
                        tracing::warn!("pre-suspend Lock() timed out; suspending anyway");
                        crate::metrics::inc_action_failure();
                    }
                }
                self.power.suspend().await.map_err(Into::into)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Recording fake: the same trait the production event loop uses.
    struct Fake {
        calls: std::sync::Mutex<Vec<Action>>,
        fail_all: bool,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                calls: Default::default(),
                fail_all: false,
            }
        }
        fn recorded(&self) -> Vec<Action> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl DispatchApi for Fake {
        async fn invoke(&self, action: Action) -> Result<()> {
            self.calls.lock().unwrap().push(action);
            if self.fail_all {
                anyhow::bail!("peer down");
            }
            Ok(())
        }
    }

    #[test]
    fn every_threshold_maps_forward() {
        assert_eq!(action_for(Threshold::Dim, true), Some(Action::Dim));
        assert_eq!(action_for(Threshold::Dpms, true), Some(Action::DpmsOff));
        assert_eq!(action_for(Threshold::Lock, true), Some(Action::Lock));
        // the 0.2.0 policy: suspend is always preceded by a lock
        assert_eq!(
            action_for(Threshold::Suspend, true),
            Some(Action::LockThenSuspend)
        );
    }

    #[test]
    fn only_dim_and_dpms_reverse() {
        assert_eq!(action_for(Threshold::Dim, false), Some(Action::Undim));
        assert_eq!(action_for(Threshold::Dpms, false), Some(Action::DpmsOn));
        assert_eq!(action_for(Threshold::Lock, false), None);
        assert_eq!(action_for(Threshold::Suspend, false), None);
    }

    #[tokio::test]
    async fn fake_receives_calls_in_order() {
        let fake = Fake::new();
        for t in [Threshold::Dim, Threshold::Lock, Threshold::Suspend] {
            if let Some(a) = action_for(t, true) {
                let _ = fake.invoke(a).await;
            }
        }
        assert_eq!(
            fake.recorded(),
            vec![Action::Dim, Action::Lock, Action::LockThenSuspend]
        );
    }

    #[tokio::test]
    async fn failure_is_reported_not_panicked() {
        let mut fake = Fake::new();
        fake.fail_all = true;
        let r = fake.invoke(Action::Lock).await;
        assert!(r.is_err());
        assert_eq!(fake.recorded(), vec![Action::Lock]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lock_deadline_is_short() {
        // guard against regressing the deadline into logind's delay
        // budget (5s default): 3s keeps headroom
        assert!(LOCK_DEADLINE < Duration::from_secs(5));
        let start = Instant::now();
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(start.elapsed() < LOCK_DEADLINE);
    }
}
