//! `org.freedesktop.ScreenSaver` compatibility.
//!
//! This is the inhibit API real applications actually call: Chrome and
//! Firefox (fullscreen video), mpv, vlc, Electron apps, presentation
//! tools. A desktop whose idle daemon doesn't answer it is a desktop
//! whose screens go dark mid-movie -- which is exactly why hypridle
//! implements it too, and why GNOME and KDE ship it somewhere in their
//! session stacks.
//!
//! Surface (the freedesktop spelling is honored exactly, including the
//! capital I in `UnInhibit`):
//!
//!   Service : org.freedesktop.ScreenSaver
//!   Paths   : /org/freedesktop/ScreenSaver  *and*  /ScreenSaver
//!             (both are in the wild; serving both is free)
//!   Inhibit(app_name: s, reason: s) -> u32 cookie
//!   UnInhibit(cookie: u32)
//!   GetActive() -> b        is the session currently blanked/locked?
//!   SetActive(b activate) -> b
//!
//! Semantics chosen for LionOS (documented, deliberate):
//! - Inhibit/UnInhibit go through the same [`InhibitGate`] as the
//!   native surface: one cookie space, one peer-death cleanup, one
//!   last-release re-arm -- so "mpv released the last cookie" behaves
//!   exactly like "a native app released the last cookie".
//! - `GetActive` reports whether the display is currently dark from our
//!   perspective (DPMS off, or a lock fired this idle period). It is
//!   *not* a promise about pixel state, which belongs to lion-power.
//! - `SetActive(true)` locks the session via `lion-locker` -- in
//!   GNOME the screensaver *is* the lock UI, and LionOS reuses that
//!   model; the return value reports whether the lock request was
//!   dispatched. `SetActive(false)` is refused (returns false): there
//!   is no un-lock without authentication, ever, by design (see
//!   lion-locker: no `Unlock()` either).
//!
//! Name ownership: requested with ReplaceExisting at startup (see
//! service.rs). If the name is later lost to another screensaver, the
//! compat surface silently stops working; that is a deployment issue
//! the logs will show via the bus, not something this daemon fights
//! over.

use std::sync::{Arc, Mutex};
use zbus::fdo;
use zbus::interface;
use zbus::message::Header;

use crate::dispatch::Locker1Proxy;
use crate::inhibit::{AddError, InhibitSource};
use crate::service::InhibitGate;

pub const SCREENSAVER_BUS: &str = "org.freedesktop.ScreenSaver";
pub const SCREENSAVER_PATH: &str = "/org/freedesktop/ScreenSaver";
pub const SCREENSAVER_LEGACY_PATH: &str = "/ScreenSaver";

/// Session display state as this daemon drives it (fed by service.rs
/// on every Wayland event that changes it).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DisplayState {
    /// DPMS-off has fired and not been resumed.
    pub dpms_off: bool,
    /// A lock has fired this idle period (and the period has not
    /// resumed since). Approximate by design: actual lock state is
    /// lion-locker's to know; this is "we asked for a lock and nobody
    /// resumed since".
    pub locked: bool,
}

impl DisplayState {
    pub fn active(&self) -> bool {
        self.dpms_off || self.locked
    }
}

pub struct ScreenSaver1 {
    gate: Arc<InhibitGate>,
    display: Arc<Mutex<DisplayState>>,
    /// Used by `SetActive(true)` to lock the session. Proxies are cheap
    /// clones sharing one connection, so the interface owns one.
    locker: Locker1Proxy<'static>,
}

impl Clone for ScreenSaver1 {
    fn clone(&self) -> Self {
        Self {
            gate: Arc::clone(&self.gate),
            display: Arc::clone(&self.display),
            locker: self.locker.clone(),
        }
    }
}

impl ScreenSaver1 {
    pub async fn new(
        gate: Arc<InhibitGate>,
        display: Arc<Mutex<DisplayState>>,
        conn: &zbus::Connection,
    ) -> zbus::Result<Self> {
        Ok(Self {
            gate,
            display,
            locker: Locker1Proxy::new(conn).await?,
        })
    }
}

#[interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver1 {
    /// freedesktop: `Inhibit(app_name, reason) -> cookie`. Note there
    /// is no "too many" error in the spec; the internal limit maps to
    /// `org.freedesktop.DBus.Error.LimitsExceeded`, the standard way
    /// to say "resource exhausted" on the bus.
    async fn inhibit(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        app_name: String,
        reason: String,
    ) -> Result<u32, fdo::Error> {
        let peer = hdr.sender().map(|s| s.to_owned());
        self.gate
            .add(peer, app_name, reason, InhibitSource::ScreenSaver)
            .await
            .map_err(|e| match e {
                AddError::LimitReached => fdo::Error::LimitsExceeded(format!(
                    "more than {} simultaneous inhibitors",
                    crate::inhibit::MAX_INHIBITORS
                )),
            })
    }

    /// freedesktop spelling is `UnInhibit` (capital I) -- zbus derives
    /// the D-Bus name from the Rust one. Same last-release re-arm as
    /// the native surface, via the shared gate.
    async fn un_inhibit(&self, cookie: u32) {
        self.gate.remove(cookie).await;
    }

    async fn get_active(&self) -> bool {
        self.display
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active()
    }

    /// `SetActive(true)`: lock the session (the screensaver *is* the
    /// lock screen in this model). Returns whether the lock request
    /// could be dispatched. `SetActive(false)` is refused by design:
    /// un-locking without authentication is not a thing this desktop
    /// does (mirrors lion-locker's lack of an `Unlock()`).
    async fn set_active(&self, activate: bool) -> bool {
        if !activate {
            tracing::warn!("org.freedesktop.ScreenSaver.SetActive(false) refused: unlocking requires authentication");
            return false;
        }
        crate::metrics::inc_screensaver_activate();
        match self.locker.lock().await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "SetActive(true): lock request failed");
                false
            }
        }
    }
}
