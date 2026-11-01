use std::io::Write as _;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use waku_protocol::{DAEMON_TOKEN_ENV, DaemonReady, PROTOCOL_VERSION};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        if arg == "mcp-agent-tools" {
            return waku_core::delegation::bridge::serve_mcp_stdio();
        }
        return run_daemon(std::iter::once(arg).chain(args));
    }
    run_daemon(std::iter::empty())
}

fn run_daemon(args: impl IntoIterator<Item = String>) -> anyhow::Result<()> {
    let arguments = Arguments::parse(args)?;
    let token =
        std::env::var(DAEMON_TOKEN_ENV).context("Waku daemon authentication token is missing")?;
    // The bearer capability belongs only to this server process. Remove it
    // before any provider or workspace subprocess can inherit the daemon's
    // environment.
    unsafe { std::env::remove_var(DAEMON_TOKEN_ENV) };
    let listener = TcpListener::bind(&arguments.bind)
        .with_context(|| format!("could not bind Waku daemon to {}", arguments.bind))?;
    let address = listener.local_addr()?;
    ensure_bind_allowed(address)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    if let Some(parent_pid) = arguments.parent_pid {
        let monitor_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("waku-daemon-parent".into())
            .spawn(move || {
                while !monitor_shutdown.load(Ordering::Acquire) {
                    if !process_is_alive(parent_pid) {
                        monitor_shutdown.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })?;
    }

    let task_path = waku_core::persistence::StateStore::default_path();
    waku_core::backup::restore_on_startup(&task_path, &waku_core::DaemonSettings::default_path())
        .context("could not finish Waku backup recovery")?;
    let settings = waku_core::DaemonSettingsStore::open_with_legacy(
        waku_core::DaemonSettings::default_path(),
        [task_path.with_file_name("settings.json")],
    )
    .context("could not load daemon settings")?;
    let task_store = waku_core::persistence::StateStore::daemon(task_path);
    let backend = Arc::new(waku_core::daemon::WakuBackend::new(settings, task_store)?);
    let ready = DaemonReady {
        address: address.to_string(),
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
    };
    println!("{}", serde_json::to_string(&ready)?);
    std::io::stdout().flush()?;
    waku_core::serve(
        listener,
        token,
        backend,
        shutdown,
        waku_core::ServerOptions {
            allow_shutdown: arguments.parent_pid.is_some(),
        },
    )
}

fn ensure_bind_allowed(address: SocketAddr) -> anyhow::Result<()> {
    if address.ip().is_loopback() {
        return Ok(());
    }
    bail!("refusing non-loopback daemon bind {address}")
}

struct Arguments {
    bind: String,
    parent_pid: Option<u32>,
}

impl Arguments {
    fn parse(arguments: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut bind = "127.0.0.1:0".to_owned();
        let mut parent_pid = None;
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--bind" => {
                    bind = arguments
                        .next()
                        .ok_or_else(|| anyhow!("--bind requires an address"))?;
                }
                "--parent-pid" => {
                    parent_pid = Some(
                        arguments
                            .next()
                            .ok_or_else(|| anyhow!("--parent-pid requires a process id"))?
                            .parse()
                            .context("--parent-pid is not a valid process id")?,
                    );
                }
                "--help" | "-h" => {
                    println!(
                        "usage: {} [--bind ADDRESS] [--parent-pid PID]",
                        env!("CARGO_BIN_NAME")
                    );
                    std::process::exit(0);
                }
                unknown => bail!("unknown argument {unknown:?}"),
            }
        }
        Ok(Self { bind, parent_pid })
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0_u32;
        let read = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        read == 0 || exit_code == STILL_ACTIVE as u32
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_loopback_listener_is_rejected() {
        assert!(ensure_bind_allowed("127.0.0.1:3000".parse().unwrap()).is_ok());
        assert!(ensure_bind_allowed("[::1]:3000".parse().unwrap()).is_ok());
        assert!(ensure_bind_allowed("0.0.0.0:3000".parse().unwrap()).is_err());
        assert!(ensure_bind_allowed("[::]:3000".parse().unwrap()).is_err());
    }
}
