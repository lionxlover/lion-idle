//! Wires everything together and serves the public D-Bus surfaces.
//!
//! `os.lionos.Idle` (native, at /os/lionos/Idle):
//!   Inhibit(app_id: s, reason: s) -> u32 (cookie)
//!       Suppress threshold actions until `Uninhibit` is called with the
//!       returned cookie, or this caller disconnects from the bus.
//!   Uninhibit(cookie: u32) -> ()
//!   GetThresholds() -> a{sv}        current thresholds (ms; absent = off)
//!   ThresholdsChanged(a{sv})         signal, on every live reload
//!   InhibitedChanged(b active, u count)  signal
//!   GetState() -> s                 JSON snapshot
//!   GetMetrics() -> s / ResetMetrics()
//!   GetCapabilities() -> as
//!
//! `org.freedesktop.ScreenSaver` (compatibility, see screensaver.rs) is
//! served by the same connection at both well-known paths, with the
//! same cookie registry. Its *name* is requested with
//! ReplaceExisting: a stale screensaver from a crashed session should
//! not keep video players from inhibiting, and on a LionOS session
//! this daemon is the intended owner.
//!
//! All inhibitor mutation -- native, compat, peer-death -- flows
//! through [`InhibitGate`], so the last-release re-arm and the
//! `InhibitedChanged` signal behave identically no matter which
//! surface a video player happened to call.
//!
//! The event loop is the one place idle events turn into actions, with
//! exactly two exits, both loud: a Wayland connection loss returns
//! `Err` (exit 1, systemd restarts, we reconnect), and a config-watcher
//! loss just freezes thresholds with a warning while the loop keeps
//! serving events (the watcher task retries by itself; see config.rs).

use crate::config::{self, Thresholds};
use crate::dispatch::{self, DispatchApi, Dispatcher};
use crate::inhibit::{AddError, InhibitSource, Inhibitors};
use crate::screensaver::{self, DisplayState};
use crate::wayland::{Threshold, WlEvent, WlHandle};
use crate::{logind, peers};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};
use zbus::names::UniqueName;
use zbus::zvariant::OwnedValue;
use zbus::{fdo, interface, object_server::SignalContext};

const IDLE_BUS: &str = "os.lionos.Idle";
const IDLE_PATH: &str = "/os/lionos/Idle";

/// The single mutation path for the inhibitor registry, shared by the
/// native surface, the screensaver compatibility surface, and the
/// peer-death watcher. Owning it here guarantees the two transitions
/// that matter behave the same everywhere:
///
/// - first inhibitor granted -> `InhibitedChanged(true, n)`;
/// - last inhibitor released (explicitly *or* by peer death) ->
///   re-arm the Wayland idle clocks and `InhibitedChanged(false, 0)`.
pub(crate) struct InhibitGate {
    inhibitors: AsyncMutex<Inhibitors>,
    wl: WlHandle,
    ctxt: SignalContext<'static>,
}

impl InhibitGate {
    pub(crate) async fn add(
        &self,
        peer: Option<UniqueName<'static>>,
        app_id: String,
        reason: String,
        source: InhibitSource,
    ) -> Result<u32, AddError> {
        let mut inhibitors = self.inhibitors.lock().await;
        let was_inhibited = inhibitors.is_inhibited();
        match inhibitors.add(peer, app_id, reason, source) {
            Ok(cookie) => {
                crate::metrics::inc_inhibit_granted(source == InhibitSource::ScreenSaver);
                if !was_inhibited {
                    emit_inhibited_changed(&self.ctxt, true, inhibitors.len()).await;
                }
                Ok(cookie)
            }
            Err(e) => {
                crate::metrics::inc_inhibit_rejected();
                Err(e)
            }
        }
    }

    /// Returns whether a live inhibitor was released (for logging).
    pub(crate) async fn remove(&self, cookie: u32) -> bool {
        let mut inhibitors = self.inhibitors.lock().await;
        let was_inhibited = inhibitors.is_inhibited();
        let removed = inhibitors.remove(cookie);
        if removed {
            crate::metrics::inc_inhibit_released(false);
        }
        if was_inhibited && inhibitors.is_empty() {
            last_inhibitor_released(&mut inhibitors, &self.wl, &self.ctxt).await;
        } else if removed {
            emit_inhibited_changed(&self.ctxt, true, inhibitors.len()).await;
        }
        removed
    }

    /// Returns how many inhibitors the departing peer held.
    pub(crate) async fn remove_peer(&self, peer: &UniqueName<'_>) -> usize {
        let mut inhibitors = self.inhibitors.lock().await;
        let released = inhibitors.remove_peer(peer);
        for _ in 0..released {
            crate::metrics::inc_inhibit_released(true);
        }
        if released > 0 && inhibitors.is_empty() {
            last_inhibitor_released(&mut inhibitors, &self.wl, &self.ctxt).await;
        } else if released > 0 {
            emit_inhibited_changed(&self.ctxt, true, inhibitors.len()).await;
        }
        released
    }

    pub(crate) async fn is_inhibited(&self) -> bool {
        self.inhibitors.lock().await.is_inhibited()
    }

    pub(crate) async fn snapshot(&self) -> (bool, usize) {
        let inhibitors = self.inhibitors.lock().await;
        (inhibitors.is_inhibited(), inhibitors.len())
    }
}

pub struct Idle1 {
    gate: Arc<InhibitGate>,
    thresholds: watch::Receiver<Thresholds>,
    display: Arc<Mutex<DisplayState>>,
    caps: Vec<String>,
}

#[interface(name = "os.lionos.Idle1")]
impl Idle1 {
    #[zbus(out_args("cookie"))]
    async fn inhibit(
        &self,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        app_id: String,
        reason: String,
    ) -> Result<u32, fdo::Error> {
        let peer = hdr.sender().map(|s| s.to_owned());
        self.gate
            .add(peer, app_id, reason, InhibitSource::Lion)
            .await
            .map_err(|e| match e {
                AddError::LimitReached => fdo::Error::LimitsExceeded(format!(
                    "more than {} simultaneous inhibitors",
                    crate::inhibit::MAX_INHIBITORS
                )),
            })
    }

    async fn uninhibit(&self, cookie: u32) {
        self.gate.remove(cookie).await;
    }

    /// Current thresholds as `a{sv}` of u64 milliseconds; a key that is
    /// absent means that threshold is disabled.
    async fn get_thresholds(&self) -> HashMap<String, OwnedValue> {
        self.thresholds.borrow().to_map()
    }

    async fn get_state(&self) -> String {
        let (inhibited, count) = self.gate.snapshot().await;
        let t = *self.thresholds.borrow();
        let (dpms_off, locked) = {
            let d = self.display.lock().unwrap_or_else(|e| e.into_inner());
            (d.dpms_off, d.locked)
        };
        fn ms(d: Option<std::time::Duration>) -> u64 {
            d.map(|d| d.as_millis().min(u64::MAX as u128) as u64)
                .unwrap_or(0)
        }
        format!(
            concat!(
                "{{",
                "\"thresholds\":{{\"dim_ms\":{},\"dpms_ms\":{},\"lock_ms\":{},\"suspend_ms\":{}}},",
                "\"inhibited\":{},\"inhibit_count\":{},",
                "\"display\":{{\"dpms_off\":{},\"locked\":{}}},",
                "\"version\":\"{}\"",
                "}}"
            ),
            ms(t.dim),
            ms(t.dpms),
            ms(t.lock),
            ms(t.suspend),
            inhibited,
            count,
            dpms_off,
            locked,
            env!("CARGO_PKG_VERSION"),
        )
    }

    async fn get_metrics(&self) -> String {
        crate::metrics::to_json()
    }

    async fn reset_metrics(&self) {
        crate::metrics::reset();
    }

    async fn get_capabilities(&self) -> Vec<String> {
        self.caps.clone()
    }

    #[zbus(signal)]
    async fn thresholds_changed(
        ctxt: &SignalContext<'_>,
        thresholds: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn inhibited_changed(
        ctxt: &SignalContext<'_>,
        active: bool,
        count: u32,
    ) -> zbus::Result<()>;
}

/// The transition that matters: the *last* inhibitor just went away,
/// possibly mid-idle-period. Re-arm the Wayland notifications so each
/// timeout restarts from now (per the protocol, a fresh notification's
/// clock starts at creation) instead of waiting for input activity
/// that may never come. Also tells subscribers.
async fn last_inhibitor_released(
    inhibitors: &mut Inhibitors,
    wl: &WlHandle,
    ctxt: &SignalContext<'_>,
) {
    debug_assert!(inhibitors.is_empty());
    // The catch-up metric is counted in wayland::rearm -- the single
    // execution point -- so logind-wake re-arms and gate re-arms are
    // counted exactly once each.
    tracing::info!("last inhibitor released; re-arming idle clocks");
    wl.rearm();
    emit_inhibited_changed(ctxt, false, 0).await;
}

async fn emit_inhibited_changed(ctxt: &SignalContext<'_>, active: bool, count: usize) {
    let _ = Idle1::inhibited_changed(ctxt, active, count as u32)
        .await
        .inspect_err(|e| {
            tracing::debug!(error = %e, "InhibitedChanged emission failed");
        });
}

async fn emit_thresholds_changed(ctxt: &SignalContext<'_>, t: &Thresholds) {
    let _ = Idle1::thresholds_changed(ctxt, t.to_map())
        .await
        .inspect_err(|e| {
            tracing::debug!(error = %e, "ThresholdsChanged emission failed");
        });
}

pub async fn serve(wl: WlHandle, mut wl_events: mpsc::UnboundedReceiver<WlEvent>) -> Result<()> {
    let conn = zbus::connection::Builder::session()?
        .name(IDLE_BUS)?
        .build()
        .await?;

    // The screensaver compatibility name is taken with ReplaceExisting
    // (see module docs): a stale owner must not break inhibit for every
    // video player on the bus. DoNotQueue: if someone else holds it and
    // we could not take it, that is worth a loud log line, not a wait.
    let dbus = fdo::DBusProxy::new(&conn).await?;
    match dbus
        .request_name(
            screensaver::SCREENSAVER_BUS.try_into()?,
            fdo::RequestNameFlags::ReplaceExisting | fdo::RequestNameFlags::DoNotQueue,
        )
        .await
    {
        Ok(_) => {}
        Err(e) => {
            // Not fatal: the native surface is the contract; the compat
            // surface is a bonus that should normally just work.
            tracing::warn!(error = %e, "could not take org.freedesktop.ScreenSaver; compatibility inhibit unavailable");
        }
    }

    let display = Arc::new(Mutex::new(DisplayState::default()));
    let ctxt = SignalContext::new(&conn, IDLE_PATH)?;
    let gate = Arc::new(InhibitGate {
        inhibitors: AsyncMutex::new(Inhibitors::default()),
        wl: wl.clone(),
        ctxt: ctxt.clone(),
    });

    // Peer-death cleanup feeds the same gate as Uninhibit.
    peers::watch_disconnects(conn.clone(), gate.clone()).await?;

    let mut thresholds_rx = config::watch_thresholds(conn.clone()).await;

    let dispatcher = Arc::new(Dispatcher::new(&conn).await?);

    // logind sleep integration degrades gracefully to a no-op (logged)
    // wherever logind is absent; capabilities report the outcome.
    let logind_ready = logind::watch(wl.clone(), dispatcher.clone()).await.is_ok();

    let idle1 = Idle1 {
        gate: gate.clone(),
        thresholds: thresholds_rx.clone(),
        display: display.clone(),
        caps: capabilities(logind_ready),
    };
    conn.object_server().at(IDLE_PATH, idle1).await?;

    // Compatibility surface, at both paths in the wild.
    let saver = screensaver::ScreenSaver1::new(gate.clone(), display.clone(), &conn).await?;
    conn.object_server()
        .at(screensaver::SCREENSAVER_PATH, saver.clone())
        .await?;
    conn.object_server()
        .at(screensaver::SCREENSAVER_LEGACY_PATH, saver)
        .await?;

    tracing::info!(bus = IDLE_BUS, path = IDLE_PATH, "IPC ready");

    let initial = *thresholds_rx.borrow();
    wl.reconfigure(initial);

    let mut thresholds_live = true;
    loop {
        tokio::select! {
            changed = thresholds_rx.changed(), if thresholds_live => {
                match changed {
                    Ok(()) => {
                        let t = *thresholds_rx.borrow();
                        tracing::info!(?t, "thresholds changed; reconfiguring Wayland notifications");
                        wl.reconfigure(t);
                        emit_thresholds_changed(&ctxt, &t).await;
                    }
                    Err(_) => {
                        // The watcher task only ends when this daemon
                        // does; treat anything else as "frozen thresholds".
                        tracing::warn!("config watcher ended; thresholds frozen at last values");
                        thresholds_live = false;
                    }
                }
            }

            event = wl_events.recv() => {
                match event {
                    Some(event) => {
                        let inhibited = gate.is_inhibited().await;
                        handle_event(event, dispatcher.as_ref(), inhibited, &display).await;
                    }
                    None => {
                        // 0.1.0 returned Ok(()) here, exiting 0 -- which
                        // Restart=on-failure reads as success. A dead
                        // Wayland connection MUST be a failure.
                        return Err(anyhow!("Wayland event stream ended (compositor connection lost)"));
                    }
                }
            }
        }
    }
}

/// One idle event, fully handled: inhibit gate, metrics, display-state
/// bookkeeping, dispatch. Generic over `DispatchApi` so the unit tests
/// drive it with a recording fake and the live tests drive the real
/// proxies.
async fn handle_event<D: DispatchApi>(
    event: WlEvent,
    dispatcher: &D,
    inhibited: bool,
    display: &Arc<Mutex<DisplayState>>,
) {
    match event {
        WlEvent::Idled(threshold) => {
            if inhibited {
                crate::metrics::inc_suppressed();
                tracing::debug!(
                    threshold = threshold.as_str(),
                    "idle action suppressed (inhibited)"
                );
                return;
            }
            crate::metrics::inc_idled(threshold.as_str());
            tracing::info!(threshold = threshold.as_str(), "idle threshold reached");
            note_display(display, threshold, true);
            if let Some(action) = dispatch::action_for(threshold, true) {
                if let Err(e) = dispatcher.invoke(action).await {
                    crate::metrics::inc_action_failure();
                    tracing::warn!(threshold = threshold.as_str(), error = %e, "action call failed");
                }
            }
        }
        WlEvent::Resumed(threshold) => {
            crate::metrics::inc_resumed();
            tracing::debug!(threshold = threshold.as_str(), "activity resumed");
            note_display(display, threshold, false);
            if let Some(action) = dispatch::action_for(threshold, false) {
                if let Err(e) = dispatcher.invoke(action).await {
                    crate::metrics::inc_action_failure();
                    tracing::warn!(threshold = threshold.as_str(), error = %e, "reverse-action call failed");
                }
            }
        }
    }
}

/// Track the coarse display state the compatibility surface reports.
/// Approximate by design (see screensaver.rs): "locked" means a lock
/// fired this idle period, cleared on the same period's resume.
fn note_display(display: &Arc<Mutex<DisplayState>>, threshold: Threshold, idle: bool) {
    let mut d = display.lock().unwrap_or_else(|e| e.into_inner());
    match (threshold, idle) {
        (Threshold::Dpms, true) => d.dpms_off = true,
        (Threshold::Dpms, false) => d.dpms_off = false,
        (Threshold::Lock, true) => d.locked = true,
        (Threshold::Lock, false) => d.locked = false,
        _ => {}
    }
}

fn capabilities(logind_ready: bool) -> Vec<String> {
    let mut caps = vec![
        "inhibit".to_owned(),
        "inhibit-peer-cleanup".to_owned(),
        "screensaver-compat".to_owned(),
        "catchup-rearm".to_owned(),
        "suspend-implies-lock".to_owned(),
        "live-config".to_owned(),
        "metrics".to_owned(),
        format!("version:{}", env!("CARGO_PKG_VERSION")),
    ];
    if logind_ready {
        caps.push("logind-sleep".to_owned());
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::Action;

    /// Recording fake dispatcher, same trait the production loop uses.
    struct Fake {
        calls: Mutex<Vec<Action>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
        fn recorded(&self) -> Vec<Action> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    impl DispatchApi for Fake {
        async fn invoke(&self, action: Action) -> Result<()> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(action);
            Ok(())
        }
    }

    #[tokio::test]
    async fn idled_maps_to_actions_when_not_inhibited() {
        let display = Arc::new(Mutex::new(DisplayState::default()));
        let fake = Fake::new();
        for t in [
            Threshold::Dim,
            Threshold::Dpms,
            Threshold::Lock,
            Threshold::Suspend,
        ] {
            handle_event(WlEvent::Idled(t), &fake, false, &display).await;
        }
        assert_eq!(
            fake.recorded(),
            vec![
                Action::Dim,
                Action::DpmsOff,
                Action::Lock,
                Action::LockThenSuspend
            ]
        );
        // display state followed along
        let d = display.lock().unwrap();
        assert!(d.dpms_off && d.locked);
    }

    #[tokio::test]
    async fn idled_is_suppressed_while_inhibited() {
        let display = Arc::new(Mutex::new(DisplayState::default()));
        let fake = Fake::new();
        handle_event(WlEvent::Idled(Threshold::Dim), &fake, true, &display).await;
        handle_event(WlEvent::Idled(Threshold::Lock), &fake, true, &display).await;
        assert!(fake.recorded().is_empty());
    }

    #[tokio::test]
    async fn resumed_reverses_dim_and_dpms_only() {
        let display = Arc::new(Mutex::new(DisplayState::default()));
        let fake = Fake::new();
        for t in [
            Threshold::Dim,
            Threshold::Dpms,
            Threshold::Lock,
            Threshold::Suspend,
        ] {
            handle_event(WlEvent::Resumed(t), &fake, false, &display).await;
        }
        assert_eq!(fake.recorded(), vec![Action::Undim, Action::DpmsOn]);
    }

    #[tokio::test]
    async fn dispatch_failure_is_contained() {
        struct Failing;
        impl DispatchApi for Failing {
            async fn invoke(&self, _action: Action) -> Result<()> {
                anyhow::bail!("peer down")
            }
        }
        let display = Arc::new(Mutex::new(DisplayState::default()));
        // must not panic, must not escape the handler
        handle_event(WlEvent::Idled(Threshold::Lock), &Failing, false, &display).await;
        handle_event(WlEvent::Resumed(Threshold::Dpms), &Failing, false, &display).await;
    }

    #[test]
    fn display_state_notes() {
        let display = Arc::new(Mutex::new(DisplayState::default()));
        note_display(&display, Threshold::Dpms, true);
        note_display(&display, Threshold::Lock, true);
        {
            let d = display.lock().unwrap();
            assert!(d.active() && d.dpms_off && d.locked);
        }
        note_display(&display, Threshold::Dpms, false);
        assert!(display.lock().unwrap().active()); // still locked
        note_display(&display, Threshold::Dim, true); // dim does not touch this state
        note_display(&display, Threshold::Lock, false);
        assert!(!display.lock().unwrap().active());
    }

    #[test]
    fn capabilities_shape() {
        let caps = capabilities(true);
        assert!(caps.contains(&"logind-sleep".to_owned()));
        assert!(caps.iter().any(|c| c.starts_with("version:")));
        let caps_no_logind = capabilities(false);
        assert!(!caps_no_logind.contains(&"logind-sleep".to_owned()));
        // the core promises survive without logind
        for core in [
            "inhibit",
            "screensaver-compat",
            "catchup-rearm",
            "suspend-implies-lock",
        ] {
            assert!(caps_no_logind.contains(&core.to_owned()), "missing {core}");
        }
    }

    /// A stub WlHandle: commands sent into it land in a channel nobody
    /// reads and wake bytes go nowhere -- enough to construct gate-adjacent
    /// types without a compositor. The end-to-end re-arm behavior is
    /// covered by the live integration suite (reconfigure actually
    /// observable there).
    fn wl_handle_stub() -> WlHandle {
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel();
        let (w, _r) = std::os::unix::net::UnixStream::pair().unwrap();
        WlHandle::for_tests(cmd_tx, std::sync::Arc::new(w))
    }

    #[test]
    fn wl_handle_stub_sends_are_inert() {
        let h = wl_handle_stub();
        // must not panic; failures are silently dropped by design
        h.reconfigure(Thresholds::default());
        h.rearm();
    }
}
