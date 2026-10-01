//! Tracks apps that have asked idle actions to be suppressed (a video
//! player during playback, a presentation app, ...), keyed by a cookie
//! handed back from `Inhibit` and consumed by `Uninhibit`.
//!
//! Cookies are also released automatically if the inhibiting peer drops
//! off the bus without calling `Uninhibit` -- a crashed app must never be
//! able to keep the screen awake forever.
//!
//! 0.2.0 hardening: both surfaces that hand out cookies
//! (`os.lionos.Idle1.Inhibit` and the `org.freedesktop.ScreenSaver`
//! compatibility interface) are reachable by *any* app in the session, so
//! the registry is now bounded (`MAX_INHIBITORS`) and the caller-supplied
//! strings are truncated (`MAX_STRING_BYTES`). Without those caps a
//! hostile or buggy app could grow the daemon's memory without limit by
//! spamming `Inhibit` with megabyte strings.
//!
//! Caveat, documented rather than hidden: `ext-idle-notify-v1` delivers
//! `idled` only once per idle period. While inhibited, an `idled` for a
//! threshold is simply not acted on; when the *last* inhibitor goes away,
//! service.rs re-arms the Wayland notifications, which per the protocol
//! restarts each timeout from that moment -- so the "video ended and the
//! user walked away without touching anything" case still locks after a
//! full threshold of quiet, instead of waiting for input activity that
//! may never come (the 0.1.0 behavior).

use std::collections::HashMap;
use zbus::names::UniqueName;

/// Hard cap on simultaneously held inhibitors. Real desktops hold a
/// handful; 128 is far above any legitimate use and far below anything
/// that could pressure memory.
pub const MAX_INHIBITORS: usize = 128;

/// Per-string cap (app id / reason), in bytes, applied before storage.
pub const MAX_STRING_BYTES: usize = 256;

/// Which public surface the inhibitor came in through. Purely for logs
/// and metrics; both sources share one cookie space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InhibitSource {
    /// `os.lionos.Idle1.Inhibit` -- native LionOS clients.
    Lion,
    /// `org.freedesktop.ScreenSaver.Inhibit` -- the freedesktop
    /// compatibility surface used by Chrome, Firefox, mpv, ...
    ScreenSaver,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddError {
    /// `MAX_INHIBITORS` cookies are already held.
    LimitReached,
}

#[derive(Default)]
pub struct Inhibitors {
    next_cookie: u32,
    by_cookie: HashMap<u32, (Option<UniqueName<'static>>, String, String, InhibitSource)>,
}

impl Inhibitors {
    /// Register one inhibitor; returns its cookie. The strings are
    /// truncated defensively; the count is enforced with an error the
    /// D-Bus layer turns into `org.freedesktop.DBus.Error.LimitsExceeded`.
    pub fn add(
        &mut self,
        peer: Option<UniqueName<'static>>,
        app_id: String,
        reason: String,
        source: InhibitSource,
    ) -> Result<u32, AddError> {
        if self.by_cookie.len() >= MAX_INHIBITORS {
            return Err(AddError::LimitReached);
        }
        // `wrapping_add` + `max(1)`: cookies start at 1 and never become 0
        // (0 would read as "unset" too easily in client bugs); after the
        // u32 wraparound the space starts over. With at most 128 live
        // cookies the reuse-collision odds are not a real concern.
        self.next_cookie = self.next_cookie.wrapping_add(1).max(1);
        let cookie = self.next_cookie;
        let app_id = truncate(app_id);
        let reason = truncate(reason);
        tracing::info!(cookie, app_id = %app_id, reason = %reason, ?source, "idle inhibited");
        self.by_cookie
            .insert(cookie, (peer, app_id, reason, source));
        Ok(cookie)
    }

    /// Release by cookie; returns whether a live inhibitor was actually
    /// dropped (callers use that to notice the last-release moment).
    pub fn remove(&mut self, cookie: u32) -> bool {
        if let Some((_, app_id, _, _)) = self.by_cookie.remove(&cookie) {
            tracing::info!(cookie, app_id = %app_id, "idle inhibit released");
            true
        } else {
            false
        }
    }

    /// Called when a peer drops off the bus; releases anything it held.
    /// Returns how many inhibitors went away.
    pub fn remove_peer(&mut self, peer: &UniqueName<'_>) -> usize {
        let mut released = 0;
        self.by_cookie.retain(|cookie, (owner, app_id, _, _)| {
            let keep = match owner {
                Some(o) => o.as_str() != peer.as_str(),
                None => true,
            };
            if !keep {
                released += 1;
                tracing::info!(cookie, app_id = %app_id, "idle inhibit released (peer disconnected)");
            }
            keep
        });
        released
    }

    pub fn is_inhibited(&self) -> bool {
        !self.by_cookie.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_cookie.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_cookie.is_empty()
    }
}

/// Cut a caller-supplied string at `MAX_STRING_BYTES` on a char boundary,
/// so no decoding panic and no unbounded storage. Logged by the caller's
/// context; this stays pure for unit tests.
pub fn truncate(mut s: String) -> String {
    if s.len() <= MAX_STRING_BYTES {
        return s;
    }
    let mut cut = MAX_STRING_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: &str) -> Option<UniqueName<'static>> {
        Some(UniqueName::try_from(n.to_owned()).unwrap())
    }

    #[test]
    fn add_remove_roundtrip() {
        let mut inh = Inhibitors::default();
        let c1 = inh
            .add(
                peer(":1.10"),
                "app.one".into(),
                "video".into(),
                InhibitSource::Lion,
            )
            .unwrap();
        let c2 = inh
            .add(
                peer(":1.11"),
                "app.two".into(),
                "call".into(),
                InhibitSource::ScreenSaver,
            )
            .unwrap();
        assert_ne!(c1, c2);
        assert!(inh.is_inhibited());
        assert_eq!(inh.len(), 2);

        assert!(inh.remove(c1));
        assert!(!inh.remove(c1)); // double release is a no-op, not an error
        assert!(inh.is_inhibited());
        assert!(inh.remove(c2));
        assert!(inh.is_empty());
    }

    #[test]
    fn cookies_never_zero() {
        let mut inh = Inhibitors::default();
        let c = inh
            .add(None, "a".into(), "r".into(), InhibitSource::Lion)
            .unwrap();
        assert_ne!(c, 0);
        // wraparound: force the counter past u32::MAX
        inh.next_cookie = u32::MAX;
        let c2 = inh
            .add(None, "a".into(), "r".into(), InhibitSource::Lion)
            .unwrap();
        assert_eq!(c2, 1); // wrapped, and 0 was skipped by .max(1)
    }

    #[test]
    fn limit_is_enforced() {
        let mut inh = Inhibitors::default();
        for i in 0..MAX_INHIBITORS {
            inh.add(
                peer(":1.1"),
                format!("app{i}"),
                "r".into(),
                InhibitSource::Lion,
            )
            .unwrap();
        }
        assert!(matches!(
            inh.add(
                peer(":1.1"),
                "one-too-many".into(),
                "r".into(),
                InhibitSource::Lion
            ),
            Err(AddError::LimitReached)
        ));
        // capacity frees up again after a release
        let any = *inh.by_cookie.keys().next().unwrap();
        assert!(inh.remove(any));
        assert!(inh
            .add(
                peer(":1.1"),
                "again".into(),
                "r".into(),
                InhibitSource::Lion
            )
            .is_ok());
    }

    #[test]
    fn peer_disconnect_releases_only_that_peer() {
        let mut inh = Inhibitors::default();
        inh.add(peer(":1.42"), "a".into(), "r".into(), InhibitSource::Lion)
            .unwrap();
        inh.add(
            peer(":1.43"),
            "b".into(),
            "r".into(),
            InhibitSource::ScreenSaver,
        )
        .unwrap();
        inh.add(None, "c".into(), "r".into(), InhibitSource::Lion)
            .unwrap(); // bus-internal caller: no owner

        let gone = UniqueName::try_from(":1.42".to_owned()).unwrap();
        let released = inh.remove_peer(&gone);
        assert_eq!(released, 1);
        assert_eq!(inh.len(), 2); // :1.43's and the ownerless one survive

        let other = UniqueName::try_from(":1.99".to_owned()).unwrap();
        assert_eq!(inh.remove_peer(&other), 0);
    }

    #[test]
    fn strings_are_truncated() {
        let long = "x".repeat(MAX_STRING_BYTES + 500);
        let mut inh = Inhibitors::default();
        let c = inh
            .add(None, long.clone(), long, InhibitSource::Lion)
            .unwrap();
        let (owner, app_id, reason, _) = inh.by_cookie.get(&c).unwrap();
        assert!(owner.is_none());
        assert_eq!(app_id.len(), MAX_STRING_BYTES);
        assert_eq!(reason.len(), MAX_STRING_BYTES);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        // multibyte input: cutting mid-char must not panic
        let s = "é".repeat(300); // 2 bytes each = 600 bytes
        let cut = truncate(s);
        assert!(cut.len() <= MAX_STRING_BYTES);
        assert!(cut.chars().all(|c| c == 'é'));

        let short = "hello".to_owned();
        assert_eq!(truncate(short.clone()), short);
    }
}
