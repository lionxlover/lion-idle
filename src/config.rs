//! Idle thresholds, read from `lion-config` and kept live.
//!
//! `lion-config`'s own build prompt (20-config.md) isn't available here,
//! so this assumes the smallest reasonable contract for a system-settings
//! service and documents it plainly rather than guessing silently:
//!
//!   Bus / path  : os.lionos.Config / /os/lionos/Config
//!   Interface   : os.lionos.Config1
//!   Get(schema: s, key: s) -> v                 current value, as a variant
//!   Changed(schema: s, key: s, value: v)         signal, fired on change
//!
//! Schema `"org.lionos.idle"`, keys `"dim-ms"`, `"dpms-ms"`, `"lock-ms"`,
//! `"suspend-ms"` -- each a `u64` of milliseconds, `0` meaning "disabled".
//!
//! If the `Get` call itself fails -- `lion-config` isn't running, or
//! doesn't recognize the schema/key yet -- that threshold falls back to
//! a safe hardcoded default rather than silently disabling: a screen
//! that never locks because a config service hiccuped is a worse failure
//! mode than one briefly using stale defaults. A value that *is*
//! returned is trusted as-is, including an explicit `0` (disabled).
//!
//! 0.2.0 changes:
//! - a returned value of the wrong type is now a *warned* disable, not a
//!   silent one (0.1.0 turned a string in `lock-ms` into "locking off"
//!   with no trace in the logs);
//! - the watcher task no longer gives up forever when `lion-config` is
//!   slow to appear or dies mid-session: it retries building the proxy
//!   with backoff and re-subscribes when the service comes back, so
//!   thresholds stay live across config-service restarts.

use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::watch;
use zbus::{proxy, zvariant::OwnedValue, Connection};

const SCHEMA: &str = "org.lionos.idle";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub dim: Option<Duration>,
    pub dpms: Option<Duration>,
    pub lock: Option<Duration>,
    pub suspend: Option<Duration>,
}

impl Default for Thresholds {
    /// Used until `lion-config` answers, and whenever a `Get` call fails
    /// -- deliberately protective: the screen still locks and the
    /// machine still suspends even with no config service in the
    /// picture.
    fn default() -> Self {
        Self {
            dim: None,
            dpms: Some(Duration::from_secs(2 * 60)),
            lock: Some(Duration::from_secs(5 * 60)),
            suspend: Some(Duration::from_secs(30 * 60)),
        }
    }
}

impl Thresholds {
    /// D-Bus-shaped map (`a{sv}`) for `GetThresholds` and the
    /// `ThresholdsChanged` signal: only *enabled* thresholds are
    /// present, as u64 milliseconds; a missing key means disabled.
    pub fn to_map(self) -> HashMap<String, OwnedValue> {
        let mut m = HashMap::new();
        let mut put = |k: &str, v: Option<Duration>| {
            if let Some(d) = v {
                m.insert(
                    k.to_owned(),
                    OwnedValue::from(d.as_millis().min(u64::MAX as u128) as u64),
                );
            }
        };
        put("dim-ms", self.dim);
        put("dpms-ms", self.dpms);
        put("lock-ms", self.lock);
        put("suspend-ms", self.suspend);
        m
    }
}

#[proxy(
    interface = "os.lionos.Config1",
    default_service = "os.lionos.Config",
    default_path = "/os/lionos/Config"
)]
trait Config1 {
    async fn get(&self, schema: &str, key: &str) -> zbus::Result<OwnedValue>;

    #[zbus(signal)]
    fn changed(&self, schema: String, key: String, value: OwnedValue) -> zbus::Result<()>;
}

/// Interpret one returned value. An explicitly-returned `0` reads as
/// "disabled"; so does a value of the wrong type -- but loudly, since
/// "lock-ms = '5m'" silently turning *off* locking is exactly the kind
/// of misconfiguration a human should hear about. Only a *failed* `Get`
/// call falls back further up, in `read_one`.
pub fn as_ms(v: OwnedValue, key: &str) -> Option<Duration> {
    // Borrow for the parse so `v` is still around to log on failure.
    let ms: u64 = (&*v).try_into().unwrap_or_else(|_| {
        tracing::warn!(key, value = ?v, "lion-config returned a non-u64 value; treating as disabled");
        0
    });
    if ms == 0 {
        tracing::debug!(key, "threshold disabled by config (0)");
        return None;
    }
    Some(Duration::from_millis(ms))
}

async fn read_one(
    proxy: &Config1Proxy<'_>,
    key: &str,
    fallback: Option<Duration>,
) -> Option<Duration> {
    match proxy.get(SCHEMA, key).await {
        Ok(v) => as_ms(v, key),
        Err(e) => {
            tracing::debug!(key, error = %e, "lion-config unavailable, using fallback");
            fallback
        }
    }
}

pub async fn read_all(proxy: &Config1Proxy<'_>) -> Thresholds {
    let d = Thresholds::default();
    Thresholds {
        dim: read_one(proxy, "dim-ms", d.dim).await,
        dpms: read_one(proxy, "dpms-ms", d.dpms).await,
        lock: read_one(proxy, "lock-ms", d.lock).await,
        suspend: read_one(proxy, "suspend-ms", d.suspend).await,
    }
}

/// One-shot read for `--print-config`: resolve thresholds from the
/// running `lion-config` if reachable, otherwise the protective
/// defaults. Returns `(thresholds, from_config_service)`.
pub async fn read_once(conn: &Connection) -> (Thresholds, bool) {
    match Config1Proxy::new(conn).await {
        Ok(proxy) => (read_all(&proxy).await, true),
        Err(e) => {
            tracing::debug!(error = %e, "lion-config not reachable for one-shot read");
            (Thresholds::default(), false)
        }
    }
}

/// Reads the current thresholds once, then keeps them live: re-reads all
/// four keys whenever any `org.lionos.idle` key changes (simpler, and
/// just as correct as matching individual keys since changes here are
/// rare -- a human adjusting a slider, not a hot loop). Returns a
/// `watch::Receiver` so `wayland.rs` can react to updates without
/// polling.
///
/// The worker task survives `lion-config` being absent at startup and
/// dying later: both cases retry with backoff and re-subscribe when the
/// service is reachable again.
pub async fn watch_thresholds(conn: Connection) -> watch::Receiver<Thresholds> {
    let (tx, rx) = watch::channel(Thresholds::default());
    tokio::spawn(async move {
        let mut attempts: u32 = 0;
        loop {
            let proxy = match Config1Proxy::new(&conn).await {
                Ok(p) => p,
                Err(e) => {
                    attempts += 1;
                    if attempts % 10 == 1 {
                        tracing::warn!(attempts, error = %e, "lion-config unreachable; thresholds are defaults until it appears");
                    }
                    tokio::time::sleep(backoff(attempts)).await;
                    continue;
                }
            };

            let initial = read_all(&proxy).await;
            tracing::info!(?initial, "idle thresholds loaded");
            if tx.send(initial).is_err() {
                return; // daemon is shutting down; nothing to serve
            }

            let Ok(mut changes) = proxy.receive_changed().await else {
                tracing::warn!("cannot subscribe to lion-config changes; retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            };
            while let Some(signal) = changes.next().await {
                let Ok(args) = signal.args() else { continue };
                if args.schema != SCHEMA {
                    continue;
                }
                let updated = read_all(&proxy).await;
                tracing::info!(?updated, changed_key = %args.key, "idle thresholds updated");
                crate::metrics::inc_config_reload();
                if tx.send(updated).is_err() {
                    return; // no receivers left; nothing to keep this task alive for
                }
            }
            // Signal stream ended: lion-config went away. Warn, then go
            // back around and wait for it to come back.
            tracing::warn!("lion-config change stream ended; will resubscribe when it returns");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    rx
}

fn backoff(attempts: u32) -> Duration {
    let secs = 1u64 << attempts.min(4); // 1s, 2s, 4s, 8s, then 16s
    Duration::from_secs(secs.min(16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn owned(v: Value) -> OwnedValue {
        v.try_into().unwrap()
    }

    #[test]
    fn as_ms_accepts_u64() {
        assert_eq!(
            as_ms(owned(Value::U64(90_000)), "lock-ms"),
            Some(Duration::from_millis(90_000))
        );
    }

    #[test]
    fn as_ms_zero_means_disabled() {
        assert_eq!(as_ms(owned(Value::U64(0)), "dim-ms"), None);
    }

    #[test]
    fn as_ms_wrong_type_is_disabled() {
        // a string where a u64 belongs: disabled (the config service
        // misbehaved) -- the *warned* branch; correctness is what's
        // asserted here, the log line is the behavior change.
        assert_eq!(as_ms(owned(Value::Str("5m".into())), "lock-ms"), None);
        assert_eq!(as_ms(owned(Value::Bool(true)), "lock-ms"), None);
    }

    #[test]
    fn as_ms_negative_is_disabled() {
        // i64 -5: not a u64; read as "disabled" rather than wrapping
        assert_eq!(as_ms(owned(Value::I64(-5)), "lock-ms"), None);
    }

    #[test]
    fn defaults_are_protective() {
        let d = Thresholds::default();
        // dim is allowed to default off; the security- and power-relevant
        // ones must be on
        assert_eq!(d.dim, None);
        assert_eq!(d.dpms, Some(Duration::from_secs(120)));
        assert_eq!(d.lock, Some(Duration::from_secs(300)));
        assert_eq!(d.suspend, Some(Duration::from_secs(1800)));
    }

    #[test]
    fn to_map_contains_only_enabled_keys() {
        let t = Thresholds {
            dim: None,
            dpms: Some(Duration::from_secs(60)),
            lock: Some(Duration::from_secs(300)),
            suspend: None,
        };
        let m = t.to_map();
        assert_eq!(m.len(), 2);
        // deref to Value, then TryFrom<&Value> for u64
        let dpms: u64 = m.get("dpms-ms").unwrap().try_into().unwrap();
        let lock: u64 = m.get("lock-ms").unwrap().try_into().unwrap();
        assert_eq!(dpms, 60_000);
        assert_eq!(lock, 300_000);
        assert!(!m.contains_key("dim-ms"));
        assert!(!m.contains_key("suspend-ms"));
    }

    #[test]
    fn backoff_is_bounded_and_monotonic() {
        let b1 = backoff(1);
        let b5 = backoff(5);
        let b99 = backoff(99);
        assert!(b1 < b5);
        assert_eq!(b5, b99);
        assert!(b99 <= Duration::from_secs(16));
    }
}
