//! Session capability registry and token validation for the Waku agent-tools MCP bridge.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use base64::Engine as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use waku_protocol::delegation::ModelReference;
use waku_protocol::workspace::ReviewDiffSource;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBridgeConfig {
    pub endpoint: String,
    pub token: String,
    pub parent_session_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_runtime_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    pub allowed_targets: Vec<ModelReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_source: Option<ReviewDiffSource>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionAlias {
    canonical_session_id: Uuid,
}

fn registration_path(directory: &Path, session_id: &str) -> PathBuf {
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(session_id);
    directory.join(format!("{encoded}.json"))
}

fn write_registration(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    use std::io::Write as _;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[derive(Clone)]
pub struct DelegationRegistry {
    directory: PathBuf,
    socket: PathBuf,
    active_sessions: Arc<Mutex<Vec<String>>>,
}

/// Unix-domain sockets cap their path at ~100 bytes, so the registry cannot
/// live under the per-user temp directory on macOS, whose path is already long.
/// `/tmp` is short, world-writable, and safe here because the directory inside
/// it is owner-only.
fn registry_base_dir() -> PathBuf {
    #[cfg(unix)]
    {
        let short = PathBuf::from("/tmp");
        if short.is_dir() {
            return short;
        }
    }
    std::env::temp_dir()
}

/// The one registry directory every Waku daemon on this host shares.
///
/// It must not be per-daemon: a provider process started by one daemon keeps
/// the registration path in its environment, and a daemon restart (the dev
/// watcher does it on every daemon edit) would otherwise strand that process
/// with a path that no longer exists — the bridge then fails with ENOENT even
/// though the new daemon registered the same session.
fn registry_directory() -> PathBuf {
    registry_base_dir().join("waku-dg")
}

/// A socket path unique to one registry instance.
///
/// The pid alone is not enough: two managers can live in one process (tests do
/// exactly that), and a shared name would make each other's `Drop` unlink a
/// socket still in use. The counter is process-local, the pid is host-wide.
fn next_socket_path(directory: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    directory.join(format!("ipc-{}-{sequence}.sock", std::process::id()))
}

/// Remove sockets left by daemons that are gone, so `/tmp` does not accumulate
/// one file per restart. Sessions files are kept: they are rewritten by the
/// next delegating turn and their path stays valid across restarts.
#[cfg(unix)]
fn sweep_dead_sockets(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_prefix("ipc-")
            .and_then(|rest| rest.strip_suffix(".sock"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if !crate::delegation::auth::process_is_alive(pid) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

impl DelegationRegistry {
    pub fn new() -> anyhow::Result<Self> {
        let directory = registry_directory();
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "could not create bridge registry directory {}",
                directory.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
            sweep_dead_sockets(&directory);
        }

        let socket = next_socket_path(&directory);
        Ok(Self {
            directory,
            socket,
            active_sessions: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// This manager's own listening socket. It is unique per instance so two
    /// daemons (debug and release, or two app instances) can share the
    /// registry directory without unlinking each other's socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn session_file_path(&self, session_id: &str) -> PathBuf {
        registration_path(&self.directory, session_id)
    }

    pub fn register_session(
        &self,
        session_id: &str,
        config: &SessionBridgeConfig,
    ) -> anyhow::Result<PathBuf> {
        let path = self.session_file_path(session_id);
        // A daemon that died between turns can have removed the directory out
        // from under this one; recreate it rather than failing the turn.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let mut merged = config.clone();
        if merged.cwd.is_none()
            && let Ok(existing_bytes) = fs::read(&path)
            && let Ok(existing) = serde_json::from_slice::<SessionBridgeConfig>(&existing_bytes)
        {
            merged.cwd = existing.cwd;
        }
        write_registration(&path, &merged)?;
        let mut active = self.active_sessions.lock();
        if !active.iter().any(|id| id == session_id) {
            active.push(session_id.to_owned());
        }
        // The bridge resolves this path from the provider's environment; when a
        // delegation fails, this line tells the two apart.
        eprintln!(
            "waku-daemon: delegation registered for session {session_id} at {}",
            path.display()
        );
        Ok(path)
    }

    pub fn unregister_session(&self, session_id: &str) {
        let path = self.session_file_path(session_id);
        let _ = fs::remove_file(path);
        self.active_sessions.lock().retain(|s| s != session_id);
        // Aliases carry no capabilities themselves. Remove their small files
        // too; an alias left by a crashed daemon still fails once canonical
        // registration is revoked.
        if let Ok(entries) = fs::read_dir(&self.directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                    && let Ok(bytes) = fs::read(&path)
                    && let Ok(alias) = serde_json::from_slice::<SessionAlias>(&bytes)
                    && alias.canonical_session_id.to_string() == session_id
                {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    /// Keep an alias to
    /// the canonical Waku registration so token/target updates remain live.
    pub fn register_alias(session_path: &Path, provider_session_id: &str) -> anyhow::Result<()> {
        let config: SessionBridgeConfig = serde_json::from_slice(&fs::read(session_path)?)?;
        let directory = session_path
            .parent()
            .context("registration has no directory")?;
        write_registration(
            &registration_path(directory, provider_session_id),
            &SessionAlias {
                canonical_session_id: config.parent_session_id,
            },
        )
    }

    pub fn is_current(&self, config: &SessionBridgeConfig) -> bool {
        if !self
            .active_sessions
            .lock()
            .iter()
            .any(|id| id == &config.parent_session_id.to_string())
        {
            return false;
        }
        Self::load_session_config(&self.directory, &config.parent_session_id.to_string()).is_ok_and(
            |current| current.token == config.token && current.endpoint == config.endpoint,
        )
    }

    pub fn load_session_config(
        directory: &Path,
        session_id: &str,
    ) -> anyhow::Result<SessionBridgeConfig> {
        let path = registration_path(directory, session_id);
        let mut bytes = fs::read(&path)
            .with_context(|| format!("no bridge registration found for session {session_id}"))?;
        if let Ok(alias) = serde_json::from_slice::<SessionAlias>(&bytes) {
            bytes = fs::read(registration_path(
                directory,
                &alias.canonical_session_id.to_string(),
            ))?;
        }
        let config: SessionBridgeConfig =
            serde_json::from_slice(&bytes).context("invalid session bridge registration file")?;
        Ok(config)
    }
}

impl Drop for DelegationRegistry {
    fn drop(&mut self) {
        // Only this daemon's socket is ours; the directory and the session
        // files outlive the process so a running provider's registration path
        // stays valid across a restart.
        let _ = fs::remove_file(&self.socket);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn aged(path: &Path) {
        let status = std::process::Command::new("touch")
            .args(["-t", "202001010000"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn the_registry_is_owner_only_and_short_enough_for_a_socket() {
        let registry = DelegationRegistry::new().unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(registry.directory())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(
            registry.socket_path().as_os_str().len() < 100,
            "unix sockets cap the path at ~100 bytes"
        );
        // The session path depends only on the session id, so a provider keeps
        // the same path across a daemon restart.
        let path = registry.session_file_path("session/with/slashes");
        assert_eq!(path.parent(), Some(registry.directory()));
        assert_eq!(
            path,
            DelegationRegistry::new()
                .unwrap()
                .session_file_path("session/with/slashes")
        );
    }

    #[test]
    fn dead_daemon_sockets_are_swept_and_live_ones_are_kept() {
        let registry = DelegationRegistry::new().unwrap();
        let directory = registry.directory().to_path_buf();

        let dead = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .status()
            .unwrap();
        assert!(dead.success());
        let dead_pid = {
            // The child has exited, so its pid names a stale socket.
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg("exit 0")
                .spawn()
                .unwrap();
            child.wait().unwrap();
            child.id()
        };
        let dead_socket = directory.join(format!("ipc-{dead_pid}-0.sock"));
        fs::write(&dead_socket, b"stale").unwrap();
        aged(&dead_socket);

        let live_socket = registry.socket_path().to_path_buf();
        fs::write(&live_socket, b"live").unwrap();

        sweep_dead_sockets(&directory);

        assert!(!dead_socket.exists(), "a dead daemon's socket is reclaimed");
        assert!(live_socket.exists(), "this daemon's own socket stays");
        let _ = fs::remove_file(&live_socket);
    }

    #[test]
    fn a_registration_round_trips_and_is_removed_on_logout() {
        let registry = DelegationRegistry::new().unwrap();
        let config = SessionBridgeConfig {
            endpoint: registry.socket_path().to_string_lossy().into_owned(),
            token: "token".into(),
            parent_session_id: Uuid::new_v4(),
            parent_turn_id: Some("turn-1".into()),
            parent_runtime_id: None,
            cwd: None,
            allowed_targets: Vec::new(),
            review_source: None,
        };
        let session = format!("session-{}", Uuid::new_v4());
        let path = registry.register_session(&session, &config).unwrap();
        assert_eq!(path, registry.session_file_path(&session));

        let loaded =
            DelegationRegistry::load_session_config(registry.directory(), &session).unwrap();
        assert_eq!(loaded.token, "token");
        assert_eq!(loaded.parent_session_id, config.parent_session_id);

        registry.unregister_session(&session);
        assert!(DelegationRegistry::load_session_config(registry.directory(), &session).is_err());
    }

    #[test]
    fn provider_aliases_follow_target_changes_and_capability_revocation() {
        let registry = DelegationRegistry::new().unwrap();
        let parent = Uuid::new_v4();
        let key = parent.to_string();
        let provider = format!("ses_{}", Uuid::new_v4());
        let mut config = SessionBridgeConfig {
            endpoint: registry.socket_path().display().to_string(),
            token: "first".into(),
            parent_session_id: parent,
            parent_turn_id: None,
            parent_runtime_id: None,
            cwd: None,
            allowed_targets: Vec::new(),
            review_source: None,
        };
        let path = registry.register_session(&key, &config).unwrap();
        DelegationRegistry::register_alias(&path, &provider).unwrap();
        assert_eq!(
            DelegationRegistry::load_session_config(registry.directory(), &provider)
                .unwrap()
                .token,
            "first"
        );
        let old = config.clone();
        config.token = "second".into();
        config.parent_turn_id = Some("new turn".into());
        registry.register_session(&key, &config).unwrap();
        let aliased =
            DelegationRegistry::load_session_config(registry.directory(), &provider).unwrap();
        assert_eq!(aliased.token, "second");
        assert_eq!(aliased.parent_turn_id, config.parent_turn_id);
        assert!(!registry.is_current(&old));
        registry.unregister_session(&key);
        assert!(!registry.is_current(&config));
        assert!(DelegationRegistry::load_session_config(registry.directory(), &provider).is_err());
    }
}
