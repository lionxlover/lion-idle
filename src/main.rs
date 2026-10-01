//! lion-idle: watches user idle time and fires configurable actions at
//! thresholds, for LionOS.
//!
//! Uses the compositor's `ext-idle-notify-v1` protocol -- one
//! notification object per configured, non-zero threshold (dim, DPMS off,
//! lock, suspend). The compositor does the actual idle detection and
//! activity reset; this daemon only reacts to `idled` / `resumed` and
//! calls the right downstream service:
//!   - dim / DPMS off           -> `lion-power`
//!   - lock                     -> `lion-locker`
//!   - suspend                  -> `lion-locker` *first* (sessions do
//!     not suspend unlocked), then `lion-power`
//!
//! Thresholds are read from `lion-config` and re-read live on change.
//! External suspends (`systemctl suspend`, lid close) are covered by
//! the logind `PrepareForSleep` watcher: lock before sleep, restore
//! the display and restart the idle clocks on wake.
//!
//! Out of scope, on purpose: the lock UI itself (`lion-locker`'s job) and
//! suspend mechanics (`lion-power`'s job) -- this only decides *when* to
//! ask for them.
//!
//! Exit codes (systemd-facing contract):
//!   0  -- only for `--version` / `--print-config`; the daemon itself
//!         never exits 0 (see below)
//!   1  -- startup failure, permanent compositor incompatibility, or
//!         Wayland connection lost mid-run: `Restart=on-failure` brings
//!         us back, and the retry loop reconnects when the compositor
//!         is ready.

mod config;
mod dispatch;
mod inhibit;
mod logind;
mod metrics;
mod peers;
mod screensaver;
mod service;
mod wayland;

use anyhow::{Context, Result};
use std::time::Duration;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lion_idle=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") => {
            println!("lion-idle {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--print-config") => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building async runtime")?
            .block_on(print_config()),
        Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some(other) => {
            print_help();
            anyhow::bail!("unknown argument: {other}")
        }
        None => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("building async runtime")?
            .block_on(run()),
    }
}

fn print_help() {
    println!(
        "lion-idle {} -- LionOS idle-watching daemon\n\n\
         Usage: lion-idle [OPTION]\n\n\
         Options:\n\
         \x20\x20--version        print version and exit\n\
         \x20\x20--print-config   resolve thresholds (lion-config, or defaults)\n\
         \x20\x20--help           this help\n\n\
         Runs as a session service (lion-idle.service); RUST_LOG=debug\n\
         for verbosity.",
        env!("CARGO_PKG_VERSION")
    );
}

async fn run() -> Result<()> {
    metrics::init_started_unix();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "lion-idle starting");

    // The compositor may not be up yet (login race): retry transient
    // connection failures in-process, with backoff, before giving the
    // failure to systemd.
    let (wl, wl_events) = wayland::spawn_with_retry()
        .await
        .context("starting Wayland idle-notify client")?;

    // serve() only returns on Wayland connection loss (as Err) or a
    // fatal IPC failure -- both exit non-zero so Restart=on-failure
    // applies. A session teardown stops us via PartOf= before that.
    service::serve(wl, wl_events).await
}

/// Resolve and print the effective thresholds. Talks to the running
/// `lion-config` when present; otherwise prints the protective
/// defaults with a note. Deliberately no failure exit: a support
/// command must always produce an answer.
async fn print_config() -> Result<()> {
    let (t, from_service) = match zbus::connection::Builder::session() {
        Ok(builder) => match builder.build().await {
            Ok(conn) => config::read_once(&conn).await,
            Err(e) => {
                eprintln!("# session bus unavailable ({e}); showing hardcoded defaults");
                (config::Thresholds::default(), false)
            }
        },
        Err(e) => {
            eprintln!("# session bus unavailable ({e}); showing hardcoded defaults");
            (config::Thresholds::default(), false)
        }
    };

    println!(
        "# source: {}",
        if from_service {
            "lion-config (live)"
        } else {
            "hardcoded defaults (lion-config unreachable)"
        }
    );
    fn line(name: &str, d: Option<Duration>) {
        match d {
            Some(d) => {
                let secs = d.as_secs_f64();
                println!("{name:<12} {:>9} ms  ({secs:.0} s)", d.as_millis());
            }
            None => println!("{name:<12} {:>12}", "disabled"),
        }
    }
    line("dim-ms", t.dim);
    line("dpms-ms", t.dpms);
    line("lock-ms", t.lock);
    line("suspend-ms", t.suspend);
    Ok(())
}
