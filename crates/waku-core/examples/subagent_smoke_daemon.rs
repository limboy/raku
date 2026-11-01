use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use waku_core::{
    DaemonReady, DaemonSettings, DaemonSettingsStore, PROTOCOL_VERSION, ServerOptions,
};

fn main() -> Result<()> {
    let directory = PathBuf::from(
        std::env::var_os("WAKU_SUBAGENT_SMOKE_DATA_DIR")
            .context("isolated smoke data directory is required")?,
    );
    std::fs::create_dir_all(&directory)?;
    let mut args = std::env::args().skip(1);
    let mut bind = "127.0.0.1:0".to_owned();
    let mut parent_pid = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => bind = args.next().context("missing bind address")?,
            "--parent-pid" => {
                parent_pid = Some(args.next().context("missing parent pid")?.parse::<i32>()?)
            }
            _ => bail!("unexpected smoke argument {arg}"),
        }
    }
    let token = std::env::var(waku_core::DAEMON_TOKEN_ENV)?;
    unsafe { std::env::remove_var(waku_core::DAEMON_TOKEN_ENV) };
    let listener = TcpListener::bind(bind)?;
    let address = listener.local_addr()?;
    ensure!(
        address.ip().is_loopback(),
        "smoke server must remain loopback-only"
    );
    let shutdown = Arc::new(AtomicBool::new(false));
    if let Some(parent_pid) = parent_pid {
        let shutdown = shutdown.clone();
        std::thread::spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                if unsafe { libc::kill(parent_pid, 0) } != 0 {
                    shutdown.store(true, Ordering::Release);
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }
    let mut settings = DaemonSettings::default();
    settings.disabled_providers = waku_core::model::ProviderKind::ALL.into_iter().collect();
    std::fs::write(
        directory.join("settings.json"),
        serde_json::to_vec(&settings)?,
    )?;
    let settings = DaemonSettingsStore::open(directory.join("settings.json"))?;
    let store = waku_core::persistence::StateStore::daemon(directory.join("app.db"));
    let backend = Arc::new(waku_core::daemon::WakuBackend::new(settings, store)?);
    println!(
        "{}",
        serde_json::to_string(&DaemonReady {
            address: address.to_string(),
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id()
        })?
    );
    std::io::stdout().flush()?;
    waku_core::serve(
        listener,
        token,
        backend,
        shutdown,
        ServerOptions {
            allow_shutdown: true,
        },
    )
}
