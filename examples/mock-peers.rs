//! Mock LionOS peers for lion-idle's live integration tests
//! (scripts/live_idle_test.sh). NOT part of the shipped daemon.
//!
//! Serves, on the (private, test) session bus:
//!   os.lionos.Locker   Lock()                        -- records "locker.Lock"
//!   os.lionos.Power    Dim/Undim/DpmsOff/DpmsOn/Suspend -- records "power.X"
//!   os.lionos.Config   Get(schema, key) / Set(key, ms) + Changed signal
//!   com.mockpeers.Log  GetLog() -> as, ClearLog(), EmitPrepareForSleep(b)
//!
//! and on the (private, test) "system" bus (reached via
//! DBUS_SYSTEM_BUS_ADDRESS):
//!   org.freedesktop.login1.Manager  Inhibit(what, who, why, mode) -> h
//!                                   PrepareForSleep(b) signal, emitted
//!                                   when the test driver calls
//!                                   EmitPrepareForSleep()
//!
//! The call log is shared by every mock, so the test script can assert
//! exact downstream behavior (ordering included) with one GetLog().

use std::collections::HashMap;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use zbus::interface;
use zbus::object_server::SignalContext;
use zbus::zvariant::OwnedValue;

#[derive(Default, Clone)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn record(&self, entry: String) {
        let mut log = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        log.push(format!("{now} {entry}"));
    }
}

struct MockLocker {
    log: Log,
}

#[interface(name = "os.lionos.Locker1")]
impl MockLocker {
    async fn lock(&self) {
        self.log.record("locker.Lock".into());
    }
}

struct MockPower {
    log: Log,
}

#[interface(name = "os.lionos.Power1")]
impl MockPower {
    async fn dim(&self) {
        self.log.record("power.Dim".into());
    }
    async fn undim(&self) {
        self.log.record("power.Undim".into());
    }
    async fn dpms_off(&self) {
        self.log.record("power.DpmsOff".into());
    }
    async fn dpms_on(&self) {
        self.log.record("power.DpmsOn".into());
    }
    async fn suspend(&self) {
        self.log.record("power.Suspend".into());
    }
}

/// Config values start as the documented defaults (etc/lionos/idle-schema.md)
/// so the daemon's initial state is deterministic; the driver mutates
/// with Set().
fn default_config() -> HashMap<String, u64> {
    HashMap::from([
        ("dim-ms".to_owned(), 0),
        ("dpms-ms".to_owned(), 120_000),
        ("lock-ms".to_owned(), 300_000),
        ("suspend-ms".to_owned(), 1_800_000),
    ])
}

struct MockConfig {
    log: Log,
    values: Mutex<HashMap<String, u64>>,
    ctxt: SignalContext<'static>,
}

#[interface(name = "os.lionos.Config1")]
impl MockConfig {
    async fn get(&self, schema: &str, key: &str) -> OwnedValue {
        self.log.record(format!("config.Get({schema},{key})"));
        let values = self.values.lock().unwrap_or_else(|e| e.into_inner());
        let ms = values.get(key).copied().unwrap_or(0);
        OwnedValue::from(ms)
    }

    /// Test-driver hook: change one key and emit the live-reload signal.
    async fn set(&self, schema: &str, key: &str, ms: u64) {
        self.log.record(format!("config.Set({schema},{key}={ms})"));
        self.values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_owned(), ms);
        let _ = MockConfig::changed(
            &self.ctxt,
            schema.to_owned(),
            key.to_owned(),
            OwnedValue::from(ms),
        )
        .await;
    }

    #[zbus(signal)]
    async fn changed(
        ctxt: &SignalContext<'_>,
        schema: String,
        key: String,
        value: OwnedValue,
    ) -> zbus::Result<()>;
}

struct MockLog {
    log: Log,
    sleep_ctxt: SignalContext<'static>,
}

#[interface(name = "com.mockpeers.Log1")]
impl MockLog {
    async fn get_log(&self) -> Vec<String> {
        self.log.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn clear_log(&self) {
        self.log.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Emit logind's PrepareForSleep on the fake system bus, exactly as
    /// logind would around a real suspend.
    async fn emit_prepare_for_sleep(&self, start: bool) -> Result<(), zbus::fdo::Error> {
        self.log
            .record(format!("login1.EmitPrepareForSleep({start})"));
        MockLogin1::prepare_for_sleep(&self.sleep_ctxt, start)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e}")))
    }
}

struct MockLogin1 {
    log: Log,
    inhibit_calls: Arc<Mutex<u32>>,
}

#[interface(name = "org.freedesktop.login1.Manager")]
impl MockLogin1 {
    /// Return a fresh (permanently EOF) pipe read-end as the inhibitor
    /// fd; the daemon should hold it for the delay and drop it on sleep.
    async fn inhibit(
        &self,
        _what: &str,
        _who: &str,
        _why: &str,
        _mode: &str,
    ) -> zbus::zvariant::OwnedFd {
        *self.inhibit_calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.log.record("login1.Inhibit(sleep,delay)".into());
        // A socketpair read-end is a perfectly valid fd to hand over
        // D-Bus; the write end is dropped, leaving a quiet EOF fd.
        let (_write, read): (UnixStream, UnixStream) = UnixStream::pair().expect("socketpair");
        let raw = read.into_raw_fd();
        let std_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        zbus::zvariant::OwnedFd::from(std_fd)
    }

    async fn inhibit_calls(&self) -> u32 {
        *self.inhibit_calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[zbus(signal)]
    async fn prepare_for_sleep(ctxt: &SignalContext<'_>, start: bool) -> zbus::Result<()>;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let log = Log::default();

    let session = zbus::connection::Builder::session()?
        .name("os.lionos.Locker")?
        .name("os.lionos.Power")?
        .name("os.lionos.Config")?
        .name("com.mockpeers.Log")?
        .build()
        .await?;

    let system = zbus::connection::Builder::system()?
        .name("org.freedesktop.login1")?
        .build()
        .await?;

    session
        .object_server()
        .at("/os/lionos/Locker", MockLocker { log: log.clone() })
        .await?;
    session
        .object_server()
        .at("/os/lionos/Power", MockPower { log: log.clone() })
        .await?;

    let config_ctxt = SignalContext::new(&session, "/os/lionos/Config")?;
    session
        .object_server()
        .at(
            "/os/lionos/Config",
            MockConfig {
                log: log.clone(),
                values: Mutex::new(default_config()),
                ctxt: config_ctxt,
            },
        )
        .await?;

    let sleep_ctxt = SignalContext::new(&system, "/org/freedesktop/login1/Manager")?;
    let inhibit_calls = Arc::new(Mutex::new(0u32));
    system
        .object_server()
        .at(
            "/org/freedesktop/login1/Manager",
            MockLogin1 {
                log: log.clone(),
                inhibit_calls: Arc::clone(&inhibit_calls),
            },
        )
        .await?;

    session
        .object_server()
        .at(
            "/mock",
            MockLog {
                log: log.clone(),
                sleep_ctxt,
            },
        )
        .await?;

    println!("mock-peers ready (session + system)");
    // Serve until the test driver kills us (SIGTERM). No need for a
    // signal handler: default disposition ends the process, which is
    // exactly the teardown the tests want.
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}
