//! Watches `org.freedesktop.DBus`'s `NameOwnerChanged` for unique names
//! (`:1.42`-style) going away, so the inhibitor registry can release
//! whatever that peer was holding without the peer having to cooperate.
//!
//! 0.2.0: everything flows through the shared [`InhibitGate`], so if a
//! peer's death drops the *last* inhibitor, the same re-arm fires as
//! for an explicit `Uninhibit` (see service.rs) -- a crashed video
//! player must not only stop suppressing idle actions, the countdown
//! must actually restart, or the "walked away after the crash" case
//! never locks.

use anyhow::Result;
use futures_util::StreamExt;
use std::sync::Arc;
use zbus::fdo::DBusProxy;
use zbus::names::UniqueName;
use zbus::Connection;

use crate::service::InhibitGate;

pub async fn watch_disconnects(conn: Connection, gate: Arc<InhibitGate>) -> Result<()> {
    let dbus = DBusProxy::new(&conn).await?;
    let mut owner_changes = dbus.receive_name_owner_changed().await?;

    tokio::spawn(async move {
        while let Some(signal) = owner_changes.next().await {
            let Ok(args) = signal.args() else { continue };
            // A unique name (":1.42"-style) whose new owner is empty just
            // disconnected; well-known name changes aren't relevant here.
            if args.new_owner.is_none() && args.name.as_str().starts_with(':') {
                if let Ok(gone) = UniqueName::try_from(args.name.as_str().to_owned()) {
                    gate.remove_peer(&gone).await;
                }
            }
        }
    });

    Ok(())
}
