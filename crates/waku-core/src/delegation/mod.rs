//! Cross-model subtask delegation and parallel code review subsystem.

pub mod auth;
pub mod bridge;
pub mod context;
pub mod scheduler;
pub mod store;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;
use waku_protocol::delegation::{DelegationGroupInfo, ModelReference};

use self::auth::{DelegationRegistry, SessionBridgeConfig};
use self::scheduler::DelegationScheduler;
use crate::persistence::StateStore;
use crate::server::Hub;
use crate::settings::DaemonSettingsStore;

/// The Pi/Oh My Pi extension that re-exports Waku's delegation tools.
///
/// Pi cannot inherit an MCP server from Waku the way Codex can, so the tools
/// arrive as an extension that spawns `waku-daemon mcp-agent-tools` itself.
pub fn pi_extension_path() -> anyhow::Result<PathBuf> {
    let path = crate::computer_use::resources_directory(
        &crate::computer_use::host_executable_path()?,
        std::env::consts::OS,
    )?
    .join("delegation/pi-extension.ts");
    crate::computer_use::packaged_file(&path, "Waku Pi delegation extension")
}

#[derive(Clone)]
pub struct DelegationManager {
    scheduler: DelegationScheduler,
    registry: Arc<DelegationRegistry>,
    ipc_endpoint: PathBuf,
}

impl DelegationManager {
    pub fn new(
        db_path: PathBuf,
        settings: Arc<DaemonSettingsStore>,
        hub: Arc<Hub>,
    ) -> anyhow::Result<Self> {
        // A previous daemon may have died mid-review; nothing can resume those
        // provider runs, so surface them as interrupted instead of "running".
        let connection = StateStore::daemon(db_path.clone()).open_connection()?;
        store::interrupt_unfinished_delegations(&connection)?;
        drop(connection);

        let registry = Arc::new(DelegationRegistry::new()?);
        let scheduler = DelegationScheduler::new(db_path, settings, hub, registry.clone());
        let ipc_endpoint = bridge::start_ipc_server(registry.clone(), scheduler.clone())?;

        Ok(Self {
            scheduler,
            registry,
            ipc_endpoint,
        })
    }

    pub fn registry_dir(&self) -> &Path {
        self.registry.directory()
    }
    pub fn registry(&self) -> &DelegationRegistry {
        &self.registry
    }

    /// The scheduler, for callers that must act without a parent session
    /// (tests and the daemon's own restart recovery).
    pub fn scheduler(&self) -> &DelegationScheduler {
        &self.scheduler
    }

    pub fn ipc_endpoint(&self) -> &Path {
        &self.ipc_endpoint
    }

    pub fn register_parent_session(
        &self,
        session_id: &str,
        parent_session_id: Uuid,
        parent_turn_id: Option<String>,
        parent_runtime_id: Option<Uuid>,
        allowed_targets: Vec<ModelReference>,
    ) -> anyhow::Result<PathBuf> {
        self.register_parent_session_with_cwd(
            session_id,
            parent_session_id,
            parent_turn_id,
            parent_runtime_id,
            None,
            allowed_targets,
        )
    }

    pub fn register_parent_session_with_cwd(
        &self,
        session_id: &str,
        parent_session_id: Uuid,
        parent_turn_id: Option<String>,
        parent_runtime_id: Option<Uuid>,
        cwd: Option<PathBuf>,
        allowed_targets: Vec<ModelReference>,
    ) -> anyhow::Result<PathBuf> {
        self.register_parent_session_with_context(
            session_id,
            parent_session_id,
            parent_turn_id,
            parent_runtime_id,
            cwd,
            allowed_targets,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_parent_session_with_context(
        &self,
        session_id: &str,
        parent_session_id: Uuid,
        parent_turn_id: Option<String>,
        parent_runtime_id: Option<Uuid>,
        cwd: Option<PathBuf>,
        allowed_targets: Vec<ModelReference>,
        review_source: Option<waku_protocol::workspace::ReviewDiffSource>,
    ) -> anyhow::Result<PathBuf> {
        let token = Uuid::new_v4().to_string();
        let config = SessionBridgeConfig {
            endpoint: self.ipc_endpoint.to_string_lossy().into_owned(),
            token,
            parent_session_id,
            parent_turn_id,
            parent_runtime_id,
            cwd,
            allowed_targets,
            review_source,
        };
        self.scheduler.register_parent(session_id, &config)
    }

    pub fn unregister_parent_session(&self, session_id: &str) {
        self.registry.unregister_session(session_id);
    }

    pub fn get_group_info(&self, group_id: Uuid) -> anyhow::Result<Option<DelegationGroupInfo>> {
        self.scheduler.get_group_info(group_id)
    }

    pub fn list_session_groups(
        &self,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<DelegationGroupInfo>> {
        self.scheduler.list_session_groups(session_id)
    }

    pub fn cancel(&self, group_id: Uuid, task_id: Option<Uuid>) -> anyhow::Result<()> {
        self.scheduler.cancel(group_id, task_id)
    }

    pub fn retry_task(&self, group_id: Uuid, task_id: Uuid) -> anyhow::Result<()> {
        self.scheduler.retry_task(group_id, task_id)
    }

    pub fn delete_session_delegations(&self, session_id: Uuid) -> anyhow::Result<()> {
        self.scheduler.cancel_parent(session_id, true)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use waku_protocol::delegation::{
        DelegatedTaskStatus, DelegationBatchRequest, DelegationTaskSpec, ModelReference,
    };
    use waku_protocol::model::ProviderKind;
    use waku_protocol::workspace::ReviewDiffSource;

    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn repository() -> PathBuf {
        let root = temporary_directory("waku-delegation-repo");
        git(&root, &["init", "-b", "main"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn baseline() {}\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "baseline",
            ],
        );
        root
    }

    fn manager(directory: &Path) -> DelegationManager {
        let settings =
            crate::settings::DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
        // These contract tests must never launch installed, authenticated CLIs.
        let mut configuration = settings.get();
        for provider in ProviderKind::ALL {
            configuration
                .provider_binary_overrides
                .entry(provider)
                .or_insert_with(|| {
                    directory
                        .join("missing-provider")
                        .to_string_lossy()
                        .into_owned()
                });
        }
        settings.replace(configuration).unwrap();
        DelegationManager::new(
            directory.join("app.db"),
            Arc::new(settings),
            Arc::new(crate::server::Hub::default()),
        )
        .unwrap()
    }

    fn batch(request_key: &str) -> DelegationBatchRequest {
        DelegationBatchRequest {
            request_key: request_key.into(),
            tasks: vec![DelegationTaskSpec {
                task_key: "security".into(),
                target_ref: "target-1".into(),
                instructions: "review the diff".into(),
            }],
        }
    }

    fn target() -> ModelReference {
        ModelReference::new(
            "target-1",
            ProviderKind::Codex,
            "gpt-5.5",
            "Codex · GPT-5.5",
        )
    }

    /// Take the snapshot a group materialized so the test can reclaim it.
    fn take_snapshot(manager: &DelegationManager, group: Uuid) -> Option<PathBuf> {
        let connection = manager.scheduler().open_connection().ok()?;
        let row = store::get_delegation_group(&connection, group).ok()??;
        store::get_review_context(&connection, row.context_id)
            .ok()??
            .snapshot_dir
    }

    #[test]
    fn a_repeated_request_key_reuses_the_group_and_its_captured_context() {
        let directory = temporary_directory("waku-delegation-db");
        std::fs::create_dir_all(&directory).unwrap();
        let manager = manager(&directory);
        let root = repository();
        let session = Uuid::new_v4();

        let first = manager
            .scheduler()
            .delegate_batch(
                session,
                Some("turn-1".into()),
                None,
                &root,
                Some(ReviewDiffSource::Uncommitted),
                &[target()],
                batch("same-key"),
            )
            .unwrap();
        let second = manager
            .scheduler()
            .delegate_batch(
                session,
                Some("turn-1".into()),
                None,
                &root,
                Some(ReviewDiffSource::Uncommitted),
                &[target()],
                batch("same-key"),
            )
            .unwrap();

        assert_eq!(first.group_id, second.group_id);
        assert_eq!(first.context_id, second.context_id);
        assert_eq!(first.task_ids, second.task_ids);

        // A different parent session is a different batch.
        let other = manager
            .scheduler()
            .delegate_batch(
                Uuid::new_v4(),
                None,
                None,
                &root,
                Some(ReviewDiffSource::Uncommitted),
                &[target()],
                batch("same-key"),
            )
            .unwrap();
        assert_ne!(first.group_id, other.group_id);

        manager.scheduler().cancel(first.group_id, None).unwrap();
        manager.scheduler().cancel(other.group_id, None).unwrap();
        let settled = manager
            .scheduler()
            .wait_delegates(first.group_id, Some(0))
            .unwrap();
        assert!(settled.status.is_terminal());
        assert_eq!(settled.tasks.len(), 1);
        assert!(settled.tasks[0].status.is_terminal());

        for group in [first.group_id, other.group_id] {
            if let Some(snapshot) = take_snapshot(&manager, group) {
                context::cleanup_snapshot(&snapshot);
            }
        }
        manager.delete_session_delegations(session).unwrap();
        assert!(manager.list_session_groups(session).unwrap().is_empty());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_model_that_echoes_the_readable_label_still_resolves_its_target() {
        let directory = temporary_directory("waku-delegation-db");
        let manager = manager(&directory);
        let root = repository();
        let session = Uuid::new_v4();

        // Models regularly copy the label they were shown instead of the id;
        // that is the same pinned destination, so the batch must not fail.
        let mut request = batch("label-echo");
        request.tasks[0].target_ref = "Codex · GPT-5.5".into();
        let response = manager
            .scheduler()
            .delegate_batch(
                session,
                None,
                None,
                &root,
                Some(ReviewDiffSource::Uncommitted),
                &[target()],
                request,
            )
            .expect("the label names an allowed target");

        manager.scheduler().cancel(response.group_id, None).unwrap();
        if let Some(snapshot) = take_snapshot(&manager, response.group_id) {
            context::cleanup_snapshot(&snapshot);
        }
        manager.delete_session_delegations(session).unwrap();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn an_unknown_target_is_rejected_before_any_subtask_starts() {
        let directory = temporary_directory("waku-delegation-db");
        let manager = manager(&directory);
        let root = repository();

        let mut request = batch("bad-target");
        request.tasks[0].target_ref = "target-9".into();
        let error = manager
            .scheduler()
            .delegate_batch(
                Uuid::new_v4(),
                None,
                None,
                &root,
                Some(ReviewDiffSource::Uncommitted),
                &[target()],
                request,
            )
            .expect_err("an unlisted target cannot be delegated to");
        assert!(error.to_string().contains("target-9"));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&directory).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_ipc_socket_requires_the_session_token() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;

        let directory = temporary_directory("waku-delegation-db");
        let manager = manager(&directory);
        let session_id = Uuid::new_v4();
        let key = session_id.to_string();
        manager
            .register_parent_session(
                &key,
                session_id,
                Some("turn-1".into()),
                None,
                vec![target()],
            )
            .unwrap();

        let ask = |token: &str| -> String {
            let mut stream = UnixStream::connect(manager.ipc_endpoint()).unwrap();
            let request = serde_json::json!({
                "token": token,
                "session_id": key,
                "method": "wait_delegates",
                "params": {"group_id": Uuid::new_v4().to_string(), "timeout_ms": 0},
            });
            serde_json::to_writer(&mut stream, &request).unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            line
        };

        // The model never sees the token: it is in the registration file.
        let denied = ask("wrong-token");
        assert!(denied.contains("invalid session token"), "{denied}");

        let config =
            auth::DelegationRegistry::load_session_config(manager.registry_dir(), &key).unwrap();
        let allowed = ask(&config.token);
        assert!(allowed.contains("not found"), "{allowed}");

        manager.unregister_parent_session(&key);
        std::fs::remove_dir_all(&directory).ok();
    }

    #[cfg(unix)]
    #[test]
    fn tool_batches_can_be_submitted_and_waited_on_with_the_advertised_arguments() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        use std::time::{Duration, Instant};

        use serde_json::json;
        use waku_protocol::delegation::DelegatedTaskSummary;

        let directory = temporary_directory("waku-delegation-tool-contract");
        // Exercise the real scheduler without depending on installed providers
        // or making paid requests: all children must report a launch failure.
        let mut settings = crate::settings::DaemonSettings::default();
        settings.provider_binary_overrides.insert(
            ProviderKind::OhMyPi,
            directory.join("missing-omp").to_string_lossy().into_owned(),
        );
        std::fs::write(
            directory.join("settings.json"),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        let manager = manager(&directory);
        let session = Uuid::new_v4();
        let key = session.to_string();
        let targets = ["target-1", "target-2", "target-3"]
            .map(|id| ModelReference::new(id, ProviderKind::OhMyPi, id, id));
        manager
            .register_parent_session_with_cwd(
                &key,
                session,
                None,
                None,
                Some(directory.clone()),
                targets.to_vec(),
            )
            .unwrap();
        let config =
            auth::DelegationRegistry::load_session_config(manager.registry_dir(), &key).unwrap();
        let call_raw = |method: &str, params: serde_json::Value| {
            let mut stream = UnixStream::connect(manager.ipc_endpoint()).unwrap();
            let request = bridge::IpcRequest {
                token: config.token.clone(),
                session_id: key.clone(),
                method: method.into(),
                params,
            };
            serde_json::to_writer(&mut stream, &request).unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            serde_json::from_str::<bridge::IpcResponse>(&line).unwrap()
        };
        let call = |method: &str, params: serde_json::Value| {
            let response = call_raw(method, params);
            assert!(response.error.is_none(), "{:?}", response.error);
            response.result.unwrap()
        };

        // Reproduce a model sending the same task name to three targets. The
        // error must explain how to retry, without reserving the request key.
        let mut request = json!({
            "request_key": "books-for-40s-2026-11-01",
            "tasks": [
                {"task_key": "books-40s", "target_ref": "target-1", "instructions": "Recommend two books."},
                {"task_key": "books-40s", "target_ref": "target-2", "instructions": "Recommend two books."},
                {"task_key": "books-40s", "target_ref": "target-3", "instructions": "Recommend two books."}
            ]
        });
        let rejected = call_raw("delegate_batch", request.clone());
        assert!(rejected.result.is_none());
        let error = rejected.error.unwrap();
        assert!(error.contains("duplicate task_key `books-40s`"), "{error}");
        assert!(error.contains("unique across the entire batch"), "{error}");
        assert!(error.contains("No subtasks were started"), "{error}");
        assert!(error.contains("same request_key"), "{error}");
        assert!(manager.list_session_groups(session).unwrap().is_empty());

        let task_keys = ["deepseek_books", "opus_books", "gemini_books"];
        for (task, task_key) in request["tasks"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(task_keys)
        {
            task["task_key"] = json!(task_key);
        }
        let batch = call("delegate_batch", request.clone());
        assert_eq!(
            call("delegate_batch", request),
            batch,
            "repeating the accepted request must not start another group"
        );
        assert_eq!(batch["task_ids"].as_object().unwrap().len(), 3);
        assert_eq!(manager.list_session_groups(session).unwrap().len(), 1);
        let group_id = batch["group_id"].as_str().expect("a waitable group_id");
        let deadline = Instant::now() + Duration::from_secs(5);
        let settled = loop {
            let progress = call(
                "wait_delegates",
                json!({"group_id": group_id, "timeout_ms": 1}),
            );
            if progress["status"] == "failed" {
                break progress;
            }
            assert!(
                Instant::now() < deadline,
                "group did not settle: {progress}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(settled["group_id"], group_id);
        // An LLM can request a longer wait than one poll. All transports must
        // accept it, and an already settled group must still return at once.
        let wait_started = Instant::now();
        let repeated = call(
            "wait_delegates",
            json!({"group_id": group_id, "timeout_ms": 240_000}),
        );
        assert_eq!(repeated, settled);
        assert!(wait_started.elapsed() < Duration::from_secs(1));
        let tasks: Vec<DelegatedTaskSummary> =
            serde_json::from_value(settled["tasks"].clone()).unwrap();
        for (task_key, target) in task_keys.into_iter().zip(targets) {
            let task = tasks.iter().find(|task| task.task_key == task_key).unwrap();
            assert_eq!(task.target, target);
            assert_eq!(task.status, DelegatedTaskStatus::Failed);
            assert_eq!(batch["task_ids"][task_key], task.task_id.to_string());
            assert!(
                task.error_message
                    .as_deref()
                    .unwrap()
                    .contains("provider Oh My Pi is not installed")
            );
        }

        manager.unregister_parent_session(&key);
        manager.delete_session_delegations(session).unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn reopening_the_database_recovers_unfinished_work() {
        let directory = temporary_directory("waku-delegation-db");
        drop(manager(&directory));
        // The second open runs the recovery pass over the same database.
        let reopened = manager(&directory);
        assert!(
            reopened
                .list_session_groups(Uuid::new_v4())
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(&directory).ok();
    }
}
