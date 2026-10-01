//! The `ext-idle-notify-v1` client. One `ext_idle_notification_v1` object
//! per configured, non-zero threshold -- the compositor does the actual
//! "has there been any input for N ms" tracking and activity reset; this
//! module only listens for `idled` / `resumed` per object and reports
//! which threshold it was.
//!
//! Like lion-locker's Wayland client, this runs its dispatch loop on a
//! dedicated OS thread and talks to the rest of the daemon over
//! channels -- wayland-client has no async story of its own.
//!
//! 0.2.0 structural changes, all fixing real 0.1.0 behavior:
//!
//! - **The loop multiplexes.** 0.1.0 drained the command channel only
//!   between `blocking_dispatch` calls, which blocks until the *next
//!   Wayland event* -- exactly the thing that stops arriving once the
//!   user is idle. A `Reconfigure` could therefore sit unprocessed for
//!   minutes. The loop now `poll(2)`s the Wayland socket *and* a wake
//!   pipe written by every command send, so reconfigures land
//!   immediately.
//! - **Connection loss is a failure.** 0.1.0's thread returned quietly,
//!   the event stream ended, `serve()` returned `Ok(())`, and the
//!   process exited 0 -- which `Restart=on-failure` correctly reads as
//!   "successful, don't restart", leaving the session with a dead idle
//!   daemon until a human intervenes. The stream now ends only on
//!   connection loss, and `service.rs` turns that into an `Err` (exit
//!   1, systemd restarts, we reconnect).
//! - **Rearm.** `Rearm` destroys and recreates every notification with
//!   the current thresholds. Per the protocol, a fresh notification's
//!   timeout starts at creation, so this is how "the last inhibitor went
//!   away mid-idle" restarts the countdown without waiting for input
//!   activity that may never come.
//! - **Startup retries transient failures.** A login race (compositor
//!   not up yet) is retried with backoff in-process; a compositor that
//!   *exists but lacks ext-idle-notify-v1* is a permanent failure and
//!   exits immediately rather than hot-looping.
//! - 0.1.0 also called `oneshot::blocking_recv()` from async context,
//!   which tokio documents as a panic; the ready handshake is now a
//!   plain `.await`.

use crate::config::Thresholds;
use anyhow::Result;
use std::io::Read;
use std::{collections::HashMap, io::Write, os::unix::net::UnixStream, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};
use wayland_client::{
    globals::GlobalListContents,
    protocol::{wl_registry, wl_seat},
    Connection as WlConnection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Threshold {
    Dim,
    Dpms,
    Lock,
    Suspend,
}

impl Threshold {
    const ALL: [Threshold; 4] = [
        Threshold::Dim,
        Threshold::Dpms,
        Threshold::Lock,
        Threshold::Suspend,
    ];

    fn duration(self, t: &Thresholds) -> Option<Duration> {
        match self {
            Threshold::Dim => t.dim,
            Threshold::Dpms => t.dpms,
            Threshold::Lock => t.lock,
            Threshold::Suspend => t.suspend,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Threshold::Dim => "dim",
            Threshold::Dpms => "dpms",
            Threshold::Lock => "lock",
            Threshold::Suspend => "suspend",
        }
    }
}

#[derive(Debug)]
pub enum WlEvent {
    Idled(Threshold),
    Resumed(Threshold),
}

pub enum WlCommand {
    /// Replace every notification object with one matching the new
    /// thresholds (destroying and recreating, since
    /// `ext-idle-notify-v1`'s timeout is fixed at creation time).
    Reconfigure(Thresholds),
    /// Recreate the notifications with the *current* thresholds: per the
    /// protocol the timeout restarts from creation, which is exactly the
    /// semantics wanted when the last inhibitor is released mid-idle.
    Rearm,
}

/// Why a Wayland client could not start. `Transient` means "try again
/// later" (compositor not up yet, mid-crash handshake); `Permanent`
/// means the compositor answered and doesn't have what we need, so
/// retrying forever would only hot-loop.
#[derive(Debug)]
pub enum SpawnError {
    Transient(anyhow::Error),
    Permanent(anyhow::Error),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::Transient(e) => write!(f, "transient: {e:#}"),
            SpawnError::Permanent(e) => write!(f, "permanent: {e:#}"),
        }
    }
}

#[derive(Clone)]
pub struct WlHandle {
    cmd_tx: std::sync::mpsc::Sender<WlCommand>,
    /// Write end of the wake pipe: one byte per command nudges the
    /// poll(2) loop out of its Wayland wait so commands are handled
    /// immediately (see module docs).
    wake_tx: Arc<UnixStream>,
}

impl WlHandle {
    /// Test-only constructor: a handle whose commands land in the given
    /// channel and whose wake bytes go to the given socket end. Production
    /// code builds handles via `spawn`/`spawn_with_retry`.
    #[cfg(test)]
    #[allow(clippy::new_ret_no_self)]
    pub fn for_tests(cmd_tx: std::sync::mpsc::Sender<WlCommand>, wake_tx: Arc<UnixStream>) -> Self {
        Self { cmd_tx, wake_tx }
    }

    pub fn reconfigure(&self, t: Thresholds) {
        self.send(WlCommand::Reconfigure(t));
    }

    pub fn rearm(&self) {
        self.send(WlCommand::Rearm);
    }

    fn send(&self, cmd: WlCommand) {
        let _ = self.cmd_tx.send(cmd);
        // A failed write only means the thread is gone (the daemon is
        // on its way out anyway); a full pipe means the thread is alive
        // but hasn't drained -- one pending wake byte is enough.
        let _ = (&*self.wake_tx).write(&[1]);
    }
}

/// Connect, with bounded retry for transient failures (the login race:
/// lion-session may start this unit a beat before the compositor
/// accepts connections). Permanent failures return immediately so
/// systemd's start-limit can do its job loudly instead of us spinning.
pub async fn spawn_with_retry() -> Result<(WlHandle, mpsc::UnboundedReceiver<WlEvent>)> {
    let mut delay = Duration::from_millis(250);
    let mut attempts: u32 = 0;
    loop {
        match spawn().await {
            Ok(pair) => return Ok(pair),
            Err(SpawnError::Permanent(e)) => {
                return Err(e.context("compositor rejected the idle-notify client permanently"));
            }
            Err(SpawnError::Transient(e)) => {
                attempts += 1;
                if attempts % 10 == 1 {
                    tracing::warn!(attempts, error = %e, "compositor not reachable yet; retrying with backoff");
                } else {
                    tracing::debug!(attempts, error = %e, "retrying compositor connection");
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        }
    }
}

pub async fn spawn() -> Result<(WlHandle, mpsc::UnboundedReceiver<WlEvent>), SpawnError> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();

    // Wake pipe: the read end is polled by the thread, the write end is
    // poked on every command.
    let (wake_write, wake_read) = UnixStream::pair()
        .map_err(|e| SpawnError::Transient(anyhow::anyhow!("socketpair: {e}")))?;
    let wake_write = Arc::new(wake_write);

    let (ready_tx, ready_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("lion-idle-wayland".into())
        .spawn(move || match State::connect(evt_tx) {
            Ok(running) => {
                let _ = ready_tx.send(Ok(()));
                running.run(cmd_rx, wake_read);
                // Reaching this point means the connection was lost; the
                // event sender drops here, which ends the stream in
                // service.rs -- and turns into a *failure* exit so
                // systemd restarts us.
                tracing::error!("Wayland client thread ended (connection lost)");
            }
            Err((e, permanent)) => {
                let _ = ready_tx.send(Err(if permanent {
                    SpawnError::Permanent(e)
                } else {
                    SpawnError::Transient(e)
                }));
            }
        })
        .map_err(|e| SpawnError::Transient(anyhow::anyhow!("spawning Wayland thread: {e}")))?;

    // Async-side handshake (0.1.0 used blocking_recv() from async
    // context, which tokio documents as a panic).
    ready_rx.await.map_err(|_| {
        SpawnError::Transient(anyhow::anyhow!("Wayland thread died before connecting"))
    })??;

    Ok((
        WlHandle {
            cmd_tx,
            wake_tx: wake_write,
        },
        evt_rx,
    ))
}

struct State {
    evt_tx: mpsc::UnboundedSender<WlEvent>,
    notifier: ExtIdleNotifierV1,
    seat: wl_seat::WlSeat,
    queue_handle: QueueHandle<State>,
    notifications: HashMap<Threshold, ExtIdleNotificationV1>,
    /// Thresholds currently reflected in `notifications` -- the source
    /// for `Rearm` (same set, fresh timeout clocks).
    current: Thresholds,
}

impl State {
    /// Errors carry a "is this permanent?" flag: failing to *connect*
    /// or a mid-handshake I/O error is transient (compositor starting
    /// up or crashing); binding failures mean the compositor answered
    /// and lacks the protocol -- permanent.
    fn connect(
        evt_tx: mpsc::UnboundedSender<WlEvent>,
    ) -> Result<RunningState, (anyhow::Error, bool)> {
        let conn = match WlConnection::connect_to_env() {
            Ok(c) => c,
            Err(e) => {
                return Err((
                    anyhow::Error::new(e).context(
                        "connecting to the Wayland compositor (is WAYLAND_DISPLAY set? lion-session should have set it)",
                    ),
                    false,
                ));
            }
        };
        let globals_result =
            wayland_client::globals::registry_queue_init::<State>(&conn).map_err(|e| {
                (
                    anyhow::Error::new(e)
                        .context("initial registry roundtrip (compositor went away?)"),
                    false,
                )
            });
        let (globals, queue) = globals_result?;
        let qh = queue.handle();

        let notifier = globals
            .bind::<ExtIdleNotifierV1, _, _>(&qh, 1..=1, ())
            .map_err(|e| {
                (
                    anyhow::Error::new(e).context("compositor does not support ext-idle-notify-v1"),
                    true,
                )
            })?;
        let seat = globals
            .bind::<wl_seat::WlSeat, _, _>(&qh, 5..=9, ())
            .map_err(|e| {
                (
                    anyhow::Error::new(e).context("no wl_seat available on this compositor"),
                    true,
                )
            })?;

        let state = State {
            evt_tx,
            notifier,
            seat,
            queue_handle: qh,
            notifications: HashMap::new(),
            current: Thresholds::default(),
        };
        Ok(RunningState { conn, queue, state })
    }
}

struct RunningState {
    conn: WlConnection,
    queue: EventQueue<State>,
    state: State,
}

impl RunningState {
    /// The event loop. poll(2) on [Wayland socket, wake pipe]; commands
    /// processed the moment they arrive rather than after the next
    /// compositor event (see module docs for why that mattered).
    fn run(mut self, cmd_rx: std::sync::mpsc::Receiver<WlCommand>, wake_read: UnixStream) {
        let mut wake_read = wake_read;
        // Non-blocking: the wake byte is drained opportunistically.
        let _ = wake_read.set_nonblocking(true);
        use rustix::event::{PollFd, PollFlags};
        use std::io::ErrorKind;

        loop {
            // 1. Handle everything already queued on our own channels.
            let mut rearm = false;
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    WlCommand::Reconfigure(t) => {
                        self.reconfigure(t);
                    }
                    WlCommand::Rearm => {
                        rearm = true;
                    }
                }
            }
            if rearm {
                self.rearm();
            }

            // 2. Push requests (destroyed/recreated notifications) out.
            match self.conn.flush() {
                Ok(()) => {}
                Err(e) if e.is_would_block() => {} // socket buffer full; retried next round
                Err(e) => {
                    tracing::warn!(error = %e, "Wayland flush failed");
                }
            }

            // 3. Dispatch anything already read into the queue.
            match self.queue.dispatch_pending(&mut self.state) {
                Ok(_n) => {}
                Err(e) => {
                    tracing::error!(error = %e, "Wayland dispatch error");
                    return;
                }
            }

            // 4. Nothing more buffered? Wait on readiness of either fd.
            //    prepare_read() returning None means events arrived in
            //    the meantime; go around again without blocking.
            let Some(guard) = self.conn.prepare_read() else {
                continue;
            };
            let wl_fd = guard.connection_fd();
            let mut fds = [
                PollFd::new(&wl_fd, PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
                PollFd::new(&wake_read, PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
            ];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => {
                    tracing::error!(error = ?e, "poll failed");
                    return;
                }
            }

            // 5. Snapshot readiness, ending the PollFd borrows before the
            //    mutable operations below.
            let wake_ready = fds[1]
                .revents()
                .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP);
            let wl_ready = fds[0]
                .revents()
                .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP);

            // 6. Drain the wake pipe (all pending bytes at once).
            if wake_ready {
                let mut buf = [0u8; 256];
                loop {
                    match wake_read.read(&mut buf) {
                        Ok(0) => break, // write end closed: treat as wake
                        Ok(_) => continue,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            }

            // 7. Read the Wayland socket if it is the one that woke us.
            if wl_ready {
                match guard.read() {
                    Ok(_n) => {}
                    Err(e) if e.is_would_block() => {}
                    Err(e) => {
                        tracing::error!(error = %e, "Wayland read failed");
                        return;
                    }
                }
            }
            // dispatch_pending happens at the top of the next iteration
        }
    }

    /// (Re)create one notification object per enabled threshold. The
    /// timeout is fixed at creation time, so both "new thresholds" and
    /// "restart the clocks" come through here.
    fn reconfigure(&mut self, t: Thresholds) {
        for (_, existing) in self.state.notifications.drain() {
            existing.destroy();
        }
        for threshold in Threshold::ALL {
            if let Some(d) = threshold.duration(&t) {
                // The protocol takes a u32 of milliseconds; a threshold
                // beyond ~49 days clamps rather than wraps.
                let ms = d.as_millis().min(u32::MAX as u128) as u32;
                let notification = self.state.notifier.get_idle_notification(
                    ms,
                    &self.state.seat,
                    &self.state.queue_handle,
                    threshold,
                );
                self.state.notifications.insert(threshold, notification);
                tracing::debug!(threshold = threshold.as_str(), ms, "watching threshold");
            }
        }
        self.state.current = t;
        self.conn.flush().ok();
    }

    /// Same thresholds, fresh timeout clocks (see `WlCommand::Rearm`).
    fn rearm(&mut self) {
        crate::metrics::inc_catchup_rearm();
        tracing::debug!("re-arming idle notifications (inhibitors drained)");
        let t = self.state.current;
        self.reconfigure(t);
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        // The globals list itself is maintained by wayland-client's
        // RegistryState machinery; the notifier and seat we bind are
        // static for the session's lifetime. (Seat replacement is
        // handled by reconnect logic, not here.)
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: ext_idle_notifier_v1::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        // We only need the seat as a handle to pass to get_idle_notification;
        // its capabilities (keyboard/pointer presence) don't affect idle
        // tracking, which is the compositor's own job.
    }
}

impl Dispatch<ExtIdleNotificationV1, Threshold> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        threshold: &Threshold,
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        let out = match event {
            ext_idle_notification_v1::Event::Idled => WlEvent::Idled(*threshold),
            ext_idle_notification_v1::Event::Resumed => WlEvent::Resumed(*threshold),
            _ => return,
        };
        let _ = state.evt_tx.send(out);
    }
}

/// Convert a `wayland_client::DispatchError`-style IO condition into a
/// would-block test (std errors only; kept small for the loop above).
trait WouldBlockExt {
    fn is_would_block(&self) -> bool;
}

impl WouldBlockExt for wayland_client::backend::WaylandError {
    fn is_would_block(&self) -> bool {
        matches!(self, wayland_client::backend::WaylandError::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    }
}

impl WouldBlockExt for std::io::Error {
    fn is_would_block(&self) -> bool {
        self.kind() == std::io::ErrorKind::WouldBlock
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_durations_map() {
        let t = Thresholds {
            dim: Some(Duration::from_millis(1000)),
            dpms: None,
            lock: Some(Duration::from_secs(60)),
            suspend: Some(Duration::from_secs(1800)),
        };
        assert_eq!(
            Threshold::Dim.duration(&t),
            Some(Duration::from_millis(1000))
        );
        assert_eq!(Threshold::Dpms.duration(&t), None);
        assert_eq!(Threshold::Lock.duration(&t), Some(Duration::from_secs(60)));
        assert_eq!(
            Threshold::Suspend.duration(&t),
            Some(Duration::from_secs(1800))
        );
    }

    #[test]
    fn threshold_names_are_stable() {
        assert_eq!(Threshold::Dim.as_str(), "dim");
        assert_eq!(Threshold::Dpms.as_str(), "dpms");
        assert_eq!(Threshold::Lock.as_str(), "lock");
        assert_eq!(Threshold::Suspend.as_str(), "suspend");
    }

    #[test]
    fn all_covers_every_threshold_exactly_once() {
        assert_eq!(Threshold::ALL.len(), 4);
        let set: std::collections::HashSet<_> = Threshold::ALL.into_iter().collect();
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn spawn_error_display_distinguishes_classes() {
        let t = SpawnError::Transient(anyhow::anyhow!("no compositor"));
        let p = SpawnError::Permanent(anyhow::anyhow!("protocol missing"));
        assert!(t.to_string().starts_with("transient:"));
        assert!(p.to_string().starts_with("permanent:"));
    }

    /// The wake pipe semantics the loop depends on: a write on one end
    /// makes the other end poll-readable, and draining it with
    /// non-blocking reads terminates cleanly at WouldBlock.
    #[test]
    fn wake_pipe_roundtrip() {
        let (w, mut r) = UnixStream::pair().unwrap();
        r.set_nonblocking(true).unwrap();
        // nothing to read yet
        assert!(
            matches!(r.read(&mut [0u8; 8]), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
        );
        (&w).write_all(&[1, 1, 1]).unwrap();
        let mut buf = [0u8; 256];
        let mut seen = 0;
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => seen += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("unexpected {e}"),
            }
        }
        assert_eq!(seen, 3);
    }
}
