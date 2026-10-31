use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::DaemonClient;
use waku_protocol::{
    APP_EXECUTABLE_ENV, Command, DAEMON_TOKEN_ENV, DaemonReady, DaemonSettings, PROTOCOL_VERSION,
    ResponsePayload,
};

const START_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const REBUILD_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct DaemonProcess {
    client: DaemonClient,
    child: Child,
}

impl DaemonProcess {
    pub fn spawn(executable: &Path) -> anyhow::Result<Self> {
        let token = Uuid::new_v4().simple().to_string();
        let app_executable = std::env::current_exe().context("could not locate Waku executable")?;
        let mut command = ProcessCommand::new(executable);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;

            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--parent-pid")
            .arg(std::process::id().to_string());

        let mut child = command
            .env(DAEMON_TOKEN_ENV, &token)
            .env(APP_EXECUTABLE_ENV, app_executable)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("could not launch {}", executable.display()))?;

        let stdout = child
            .stdout
            .take()
            .context("Waku daemon did not expose its readiness stream")?;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("waku-daemon-ready".into())
            .spawn(move || {
                let mut line = String::new();
                let result = BufReader::new(stdout)
                    .read_line(&mut line)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| {
                        if bytes == 0 {
                            bail!("Waku daemon exited before becoming ready")
                        }
                        serde_json::from_str::<DaemonReady>(&line).map_err(anyhow::Error::from)
                    });
                let _ = ready_tx.send(result);
            })
            .context("could not start Waku daemon readiness reader")?;

        let restoring = crate::persistence::StateStore::default_path()
            .with_file_name(waku_protocol::backup::READY_RESTORE)
            .exists()
            || crate::persistence::StateStore::default_path()
                .with_file_name(".restore-journal.json")
                .exists();
        let timeout = if restoring {
            Duration::from_secs(30 * 60)
        } else {
            START_TIMEOUT
        };
        let ready = match ready_rx.recv_timeout(timeout) {
            Ok(Ok(ready)) => ready,
            Ok(Err(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("timed out waiting for Waku daemon: {error}");
            }
        };
        if ready.protocol_version != PROTOCOL_VERSION {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "daemon protocol {} does not match desktop protocol {}",
                ready.protocol_version,
                PROTOCOL_VERSION
            );
        }
        let client_address = match desktop_client_address(&ready.address) {
            Ok(address) => address,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let client = match DaemonClient::connect(&client_address, token) {
            Ok(client) => client,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self { client, child })
    }

    pub fn client(&self) -> DaemonClient {
        self.client.clone()
    }

    fn has_exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    fn stop(&mut self) {
        self.client.shutdown();
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn desktop_client_address(address: &str) -> anyhow::Result<String> {
    let address = address
        .parse::<std::net::SocketAddr>()
        .with_context(|| format!("Waku daemon returned an invalid address {address:?}"))?;
    let ip = if address.ip().is_unspecified() {
        if address.is_ipv4() {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
    } else {
        address.ip()
    };
    Ok(std::net::SocketAddr::new(ip, address.port()).to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExecutableStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl ExecutableStamp {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("could not inspect {}", path.display()))?;
        Ok(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

struct SupervisorInner {
    executable: PathBuf,
    target: Mutex<DaemonTarget>,
    restart: Mutex<()>,
    settings: Mutex<DaemonSettings>,
    persisted_settings: Mutex<Option<DaemonSettings>>,
    settings_updates: Sender<DaemonSettings>,
    client_updates: Mutex<Vec<Sender<DaemonClient>>>,
    running: AtomicBool,
}

enum DaemonTarget {
    Local(DaemonProcess),
    Restarting(DaemonClient),
}

impl DaemonTarget {
    fn client(&self) -> DaemonClient {
        match self {
            Self::Local(process) => process.client(),
            Self::Restarting(client) => client.clone(),
        }
    }
}

#[derive(Clone)]
pub struct DaemonSupervisor {
    inner: Arc<SupervisorInner>,
}

impl DaemonSupervisor {
    pub fn spawn(executable: &Path, watch_for_rebuilds: bool) -> anyhow::Result<Self> {
        let process = DaemonProcess::spawn(executable)?;
        let settings = read_settings(&process.client())?;
        let initial_stamp = ExecutableStamp::read(executable)?;
        let (settings_updates, settings_update_rx) = unbounded();
        let inner = Arc::new(SupervisorInner {
            executable: executable.to_owned(),
            target: Mutex::new(DaemonTarget::Local(process)),
            restart: Mutex::new(()),
            settings: Mutex::new(settings),
            persisted_settings: Mutex::new(None),
            settings_updates,
            client_updates: Mutex::new(Vec::new()),
            running: AtomicBool::new(true),
        });
        let weak_inner = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("waku-daemon-settings".into())
            .spawn(move || persist_settings(weak_inner, settings_update_rx))
            .context("could not start Waku daemon settings writer")?;

        let supervisor = Self { inner };
        let weak_inner = Arc::downgrade(&supervisor.inner);
        std::thread::Builder::new()
            .name("waku-daemon-supervisor".into())
            .spawn(move || monitor_daemon(weak_inner, Some(initial_stamp), watch_for_rebuilds))
            .context("could not start Waku daemon supervisor")?;
        Ok(supervisor)
    }

    pub fn client(&self) -> DaemonClient {
        self.inner.target.lock().client()
    }

    pub fn subscribe_clients(&self) -> Receiver<DaemonClient> {
        let (updates, receiver) = unbounded();
        let target = self.inner.target.lock();
        self.inner.client_updates.lock().push(updates.clone());
        let _ = updates.send(target.client());
        receiver
    }

    pub fn settings(&self) -> DaemonSettings {
        self.inner.settings.lock().clone()
    }

    pub fn shutdown_for_relaunch(&self) -> anyhow::Result<()> {
        let _restart = self.inner.restart.lock();
        self.inner.running.store(false, Ordering::Release);
        let previous = {
            let mut target = self.inner.target.lock();
            let disconnected = target.client();
            std::mem::replace(&mut *target, DaemonTarget::Restarting(disconnected))
        };
        drop(previous);
        Ok(())
    }

    pub fn update_settings(&self, settings: DaemonSettings) -> anyhow::Result<()> {
        *self.inner.settings.lock() = settings.clone();
        if self.inner.persisted_settings.lock().as_ref() == Some(&settings) {
            return Ok(());
        }
        self.inner
            .settings_updates
            .send(settings)
            .map_err(|_| anyhow::anyhow!("Waku daemon settings writer is closed"))
    }
}

impl Drop for DaemonSupervisor {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.inner.running.store(false, Ordering::Release);
        }
    }
}

fn monitor_daemon(
    weak_inner: std::sync::Weak<SupervisorInner>,
    mut active_stamp: Option<ExecutableStamp>,
    watch_for_rebuilds: bool,
) {
    loop {
        std::thread::sleep(REBUILD_POLL_INTERVAL);
        let Some(inner) = weak_inner.upgrade() else {
            return;
        };
        if !inner.running.load(Ordering::Acquire) {
            return;
        }

        let process_exited = {
            let mut target = inner.target.lock();
            match &mut *target {
                DaemonTarget::Local(process) => process.has_exited(),
                // A failed spawn leaves no child to poll. Keep retrying even
                // when executable rebuild watching is disabled in Release.
                DaemonTarget::Restarting(_) => true,
            }
        };

        let executable = &inner.executable;
        let observed_stamp = ExecutableStamp::read(executable).ok();
        let executable_changed = watch_for_rebuilds
            && observed_stamp.is_some_and(|observed| Some(observed) != active_stamp);
        if !process_exited && !executable_changed {
            continue;
        }
        let _restart = inner.restart.lock();
        if !inner.running.load(Ordering::Acquire) {
            return;
        }
        match replace_local_daemon(&inner, executable) {
            Ok(()) => {}
            Err(error) => {
                eprintln!("could not restart rebuilt Waku daemon: {error:#}");
                continue;
            }
        }
        queue_settings_refresh(&inner);
        if let Some(observed_stamp) = observed_stamp {
            active_stamp = Some(observed_stamp);
        }
        drop(_restart);
        drop(inner);
    }
}

fn replace_local_daemon(inner: &SupervisorInner, executable: &Path) -> anyhow::Result<()> {
    if !inner.running.load(Ordering::Acquire) {
        bail!("the daemon supervisor has been stopped");
    }
    let previous = {
        let mut target = inner.target.lock();
        match &*target {
            DaemonTarget::Restarting(_) => None,
            DaemonTarget::Local(process) => {
                let disconnected = process.client();
                let previous =
                    std::mem::replace(&mut *target, DaemonTarget::Restarting(disconnected));
                match previous {
                    DaemonTarget::Local(process) => Some(process),
                    _ => unreachable!("local daemon target changed while locked"),
                }
            }
        }
    };
    drop(previous);
    let replacement = DaemonProcess::spawn(executable)?;
    let client = replacement.client();
    *inner.target.lock() = DaemonTarget::Local(replacement);
    inner
        .client_updates
        .lock()
        .retain(|subscriber| subscriber.send(client.clone()).is_ok());
    Ok(())
}

fn queue_settings_refresh(inner: &SupervisorInner) {
    let settings = inner.settings.lock().clone();
    *inner.persisted_settings.lock() = None;
    let _ = inner.settings_updates.send(settings);
}

fn read_settings(client: &DaemonClient) -> anyhow::Result<DaemonSettings> {
    match client.request(Uuid::nil(), Uuid::nil(), Command::GetSettings)? {
        ResponsePayload::Settings { settings } => Ok(settings),
        _ => bail!("Waku daemon returned an invalid settings response"),
    }
}

fn persist_settings(
    weak_inner: std::sync::Weak<SupervisorInner>,
    updates: Receiver<DaemonSettings>,
) {
    while let Ok(mut settings) = updates.recv() {
        while let Ok(newer) = updates.try_recv() {
            settings = newer;
        }
        loop {
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            if !inner.running.load(Ordering::Acquire) {
                return;
            }
            let desired = inner.settings.lock().clone();
            if desired != settings {
                settings = desired;
            }
            let client = inner.target.lock().client();
            let result = client.request(
                Uuid::nil(),
                Uuid::nil(),
                Command::UpdateSettings {
                    settings: settings.clone(),
                },
            );
            match result {
                Ok(ResponsePayload::Ack) => {
                    *inner.persisted_settings.lock() = Some(settings);
                    break;
                }
                Ok(_) => {
                    eprintln!("Waku daemon returned an invalid settings update response");
                }
                Err(error) => {
                    eprintln!("could not persist Waku daemon settings: {error:#}");
                }
            }
            drop(inner);
            std::thread::sleep(REBUILD_POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn failed_restart_recovers_without_watching_for_rebuilds() {
        use std::net::{TcpListener, TcpStream};
        use std::os::unix::fs::PermissionsExt as _;
        use waku_protocol::{ClientMessage, ResponseOutcome, ServerMessage};

        let directory = std::env::temp_dir().join(format!("waku-restart-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("daemon");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let ready = serde_json::to_string(&DaemonReady {
            address: address.to_string(),
            protocol_version: PROTOCOL_VERSION,
            pid: 0,
        })
        .unwrap();
        // The second launch fails once; the third can serve requests again.
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\n\
                 if [ -f \"$0.started\" ] && [ ! -f \"$0.failed\" ]; then\n\
                   : > \"$0.failed\"\n\
                   exit 1\n\
                 fi\n\
                 : > \"$0.started\"\n\
                 printf '%s\\n' '{ready}'\n\
                 exec /bin/sleep 30\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();

        let stopped = Arc::new(AtomicBool::new(false));
        let server_stopped = stopped.clone();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let stream = stream.unwrap();
                if server_stopped.load(Ordering::Acquire) {
                    return;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                loop {
                    let message: ClientMessage =
                        serde_json::from_str(&socket.read().unwrap().into_text().unwrap()).unwrap();
                    let response = match message {
                        ClientMessage::Hello { .. } => ServerMessage::Hello {
                            protocol_version: PROTOCOL_VERSION,
                            daemon_version: "test".into(),
                        },
                        ClientMessage::Request(request) => ServerMessage::Response {
                            request_id: request.request_id,
                            outcome: ResponseOutcome::Ok {
                                payload: match request.command {
                                    Command::GetSettings => ResponsePayload::Settings {
                                        settings: DaemonSettings::default(),
                                    },
                                    Command::UpdateSettings { .. } => ResponsePayload::Ack,
                                    _ => panic!("unexpected test daemon command"),
                                },
                            },
                        },
                        ClientMessage::Shutdown => break,
                    };
                    socket
                        .send(tungstenite::Message::Text(
                            serde_json::to_string(&response).unwrap().into(),
                        ))
                        .unwrap();
                }
            }
        });

        let supervisor = DaemonSupervisor::spawn(&executable, false).unwrap();
        let clients = supervisor.subscribe_clients();
        let initial = clients.recv_timeout(Duration::from_secs(1)).unwrap();
        if let DaemonTarget::Local(process) = &mut *supervisor.inner.target.lock() {
            process.child.kill().unwrap();
        }
        let replacement = clients.recv_timeout(Duration::from_secs(5));
        let settings = replacement.as_ref().ok().map(read_settings);
        let failed_once = directory.join("daemon.failed").exists();

        // Unblock accept on failure so a regressed monitor leaves no server
        // thread or child behind when the assertions below fail.
        stopped.store(true, Ordering::Release);
        supervisor.shutdown_for_relaunch().unwrap();
        let _ = TcpStream::connect(address);
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();

        assert!(failed_once, "the fixture must exercise a failed restart");
        let replacement = replacement.expect("daemon should recover after a failed restart");
        assert!(!initial.same_connection(&replacement));
        assert!(
            matches!(settings, Some(Ok(_))),
            "replacement must serve RPCs"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_relaunch_stops_the_old_process_without_blocking_ui_readers() {
        use crossbeam_channel::bounded;
        use std::net::TcpListener;
        use waku_protocol::{ClientMessage, ServerMessage};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut child = ProcessCommand::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let input = child.stdin.take().unwrap();
        let (shutdown_seen, shutdown) = bounded(1);
        let (release, release_rx) = bounded(1);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let hello: ClientMessage =
                serde_json::from_str(&socket.read().unwrap().into_text().unwrap()).unwrap();
            assert!(matches!(hello, ClientMessage::Hello { .. }));
            socket
                .send(tungstenite::Message::Text(
                    serde_json::to_string(&ServerMessage::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        daemon_version: "test".into(),
                    })
                    .unwrap()
                    .into(),
                ))
                .unwrap();
            let message: ClientMessage =
                serde_json::from_str(&socket.read().unwrap().into_text().unwrap()).unwrap();
            assert!(matches!(message, ClientMessage::Shutdown));
            shutdown_seen.send(()).unwrap();
            let _ = release_rx.recv();
            drop(input);
        });
        let client = DaemonClient::connect(&address.to_string(), "test-token".into()).unwrap();
        let (settings_updates, _settings_rx) = unbounded();
        let supervisor = DaemonSupervisor {
            inner: Arc::new(SupervisorInner {
                executable: PathBuf::from("/bin/cat"),
                target: Mutex::new(DaemonTarget::Local(DaemonProcess { client, child })),
                restart: Mutex::new(()),
                settings: Mutex::new(DaemonSettings::default()),
                persisted_settings: Mutex::new(None),
                settings_updates,
                client_updates: Mutex::new(Vec::new()),
                running: AtomicBool::new(true),
            }),
        };
        let stopping = supervisor.clone();
        let stop = std::thread::spawn(move || stopping.shutdown_for_relaunch());
        shutdown.recv_timeout(Duration::from_secs(2)).unwrap();
        let target = supervisor
            .inner
            .target
            .try_lock()
            .expect("UI client store must stay available");
        assert!(matches!(&*target, DaemonTarget::Restarting(_)));
        drop(target);
        assert!(!supervisor.inner.running.load(Ordering::Acquire));
        release.send(()).unwrap();
        stop.join().unwrap().unwrap();
        server.join().unwrap();
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn desktop_uses_loopback_to_reach_an_unspecified_listener() {
        assert_eq!(
            desktop_client_address("0.0.0.0:34123").unwrap(),
            "127.0.0.1:34123"
        );
        assert_eq!(desktop_client_address("[::]:34123").unwrap(), "[::1]:34123");
    }
}
