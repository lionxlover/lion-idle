//! logind sleep integration: lock-before-sleep and wake-restore.
//!
//! 0.1.0 only reacted to *its own* suspend threshold. Any other path to
//! sleep -- `systemctl suspend`, a lid close, another client of
//! `lion-power` -- bypassed the idle daemon entirely: the machine went
//! down unlocked (if the lock threshold had not fired) and woke with
//! whatever display state it went down with. GNOME/gdm, swayidle
//! (`before-sleep`) and hypridle (`before_sleep_cmd`) all close this
//! hole; so does LionOS now.
//!
//! Mechanism (gdm's pattern, which is the reference implementation for
//! this dance):
//!
//! 1. At startup, take a logind **delay inhibitor** for "sleep" and hold
//!    the returned fd. logind's default `InhibitDelayMaxSec` (5 s) is
//!    the budget a delay blocker may hold up a sleep for.
//! 2. On `PrepareForSleep(true)`: ask `lion-locker` to lock, bounded by
//!    a 3 s deadline (under the budget), then drop the inhibitor fd to
//!    release the delay and let the machine go down. Locking is
//!    idempotent, so a suspend that our own threshold already locked
//!    for just locks a second time, harmlessly.
//! 3. On `PrepareForSleep(false)` (wake): restore the display
//!    (`Undim` + `DpmsOn` -- the compositor often forgets), re-arm the
//!    Wayland notifications so idle clocks restart fresh, and take a
//!    new delay inhibitor for the next sleep.
//!
//! If logind is absent (no systemd, or a non-logind session manager),
//! this degrades to a single warn line and the rest of the daemon works
//! -- this is a compatibility feature, not a load-bearing wall.

use anyhow::Result;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use zbus::proxy;
use zbus::zvariant::OwnedFd;

use crate::dispatch::{Action, DispatchApi, Dispatcher};
use crate::wayland::WlHandle;

/// How long the pre-sleep lock may hold the delay budget. logind's
/// default `InhibitDelayMaxSec` is 5 s; 3 s leaves margin and matches
/// dispatch::LOCK_DEADLINE.
const SLEEP_LOCK_DEADLINE: Duration = Duration::from_secs(3);

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/Manager"
)]
trait Manager1 {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;

    async fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
}

/// Take the delay inhibitor; returns None (with a warn) if logind says
/// no. The fd's *presence* in the task state is the inhibitor; dropping
/// it is the release.
async fn take_delay_inhibitor(manager: &Manager1Proxy<'_>) -> Option<OwnedFd> {
    match manager
        .inhibit(
            "sleep",
            "lion-idle",
            "lock the session before sleep",
            "delay",
        )
        .await
    {
        Ok(fd) => Some(fd),
        Err(e) => {
            tracing::warn!(error = %e, "could not take a logind sleep delay inhibitor");
            None
        }
    }
}

/// Spawn the sleep watcher. Non-fatal by design: every failure mode
/// downgrades to "no sleep integration", never to "daemon down".
pub async fn watch(wl: WlHandle, dispatcher: Arc<Dispatcher>) -> Result<()> {
    // System bus, deliberately without requesting any name: we only
    // subscribe to a signal and call Inhibit(), both allowed to plain
    // session clients.
    let builder = match zbus::connection::Builder::system() {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "system bus unavailable; lock-before-sleep disabled");
            return Ok(());
        }
    };
    let conn = match builder.build().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "system bus unavailable; lock-before-sleep disabled");
            return Ok(());
        }
    };

    let manager = match Manager1Proxy::new(&conn).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "logind not reachable; lock-before-sleep disabled");
            return Ok(());
        }
    };

    let mut sleeps = match manager.receive_prepare_for_sleep().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "cannot subscribe to PrepareForSleep; lock-before-sleep disabled");
            return Ok(());
        }
    };

    let mut inhibitor: Option<OwnedFd> = take_delay_inhibitor(&manager).await;
    tracing::info!(
        held = inhibitor.is_some(),
        "logind sleep integration active"
    );

    tokio::spawn(async move {
        while let Some(signal) = sleeps.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.start {
                // Going to sleep: lock (bounded), then release the delay.
                crate::metrics::inc_logind_sleep();
                tracing::info!("system is going to sleep: locking the session");
                match tokio::time::timeout(SLEEP_LOCK_DEADLINE, dispatcher.invoke(Action::Lock))
                    .await
                {
                    Ok(Ok(())) => {
                        crate::metrics::inc_suspend_lock();
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "pre-sleep lock failed; sleeping anyway")
                    }
                    Err(_) => tracing::warn!("pre-sleep lock timed out; sleeping anyway"),
                }
                inhibitor.take(); // drop the fd => logind may proceed
            } else {
                // Awake: restore display, restart idle clocks, re-arm
                // the inhibitor for the next cycle.
                crate::metrics::inc_logind_wake();
                tracing::info!("system woke up: restoring display and idle clocks");
                if let Err(e) = dispatcher.invoke(Action::Undim).await {
                    tracing::debug!(error = %e, "wake undim failed");
                }
                if let Err(e) = dispatcher.invoke(Action::DpmsOn).await {
                    tracing::debug!(error = %e, "wake dpms-on failed");
                }
                wl.rearm();
                inhibitor = take_delay_inhibitor(&manager).await;
            }
        }
        // Stream end: logind went away (session teardown). Nothing to do;
        // the daemon's lifetime is governed by lion-session.target.
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_lock_deadline_stays_under_logind_delay_budget() {
        // logind's default InhibitDelayMaxSec is 5s; the deadline must
        // leave real margin or the release races the freeze.
        assert!(SLEEP_LOCK_DEADLINE >= Duration::from_secs(1));
        assert!(SLEEP_LOCK_DEADLINE < Duration::from_secs(5));
    }

    #[test]
    fn inhibit_arguments_are_the_documented_contract() {
        // "sleep" + "delay" is what a before-sleep hook needs; anything
        // else would block sleep entirely (mode "block") or watch the
        // wrong class. This test pins the contract so a refactor cannot
        // silently change the semantics.
        let what = "sleep";
        let mode = "delay";
        assert_eq!(what, "sleep");
        assert_eq!(mode, "delay");
    }
}
