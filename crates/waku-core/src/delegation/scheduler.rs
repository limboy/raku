//! Concurrency scheduler and task runner for delegated subtasks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use parking_lot::{Condvar, Mutex};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use waku_protocol::delegation::{
    DelegatedTaskStatus, DelegationBatchRequest, DelegationBatchResponse, DelegationGroupInfo,
    DelegationGroupStatus, ModelReference, WaitDelegatesResponse,
};
use waku_protocol::model::{DriverEvent, ExecutionPolicy, ProviderKind, RuntimeMode, unix_time};
use waku_protocol::workspace::ReviewDiffSource;
use waku_protocol::{WireDriverEvent, event_to_wire};

use super::auth::{DelegationRegistry, SessionBridgeConfig};
use super::context::{CapturedReviewContext, SnapshotCleanup, capture_review_context};
use super::store::{
    self, DelegatedTaskRow, DelegationGroupRow, finish_delegated_task, get_delegation_group,
    get_delegation_group_by_request_key, get_review_context, insert_delegated_task,
    insert_delegation_group, insert_review_context, list_delegated_tasks_by_group,
    list_delegation_groups_by_session, update_delegated_task_progress,
    update_delegation_group_status,
};
use crate::driver::{self, DriverHandle, DriverStartOptions};
use crate::persistence::StateStore;
use crate::server::Hub;
use crate::settings::DaemonSettingsStore;

const MAX_TASKS_PER_GROUP: usize = 4;
const MAX_CONCURRENT_PER_GROUP: usize = 2;
const MAX_GLOBAL_CONCURRENT: usize = 4;
const TASK_TIMEOUT: Duration = Duration::from_secs(15 * 60);

struct ActiveChild {
    handle: DriverHandle,
    runtime_id: Uuid,
    cancelled: Arc<AtomicBool>,
}

/// Counting gate over the daemon-wide child budget.
struct ExecutionGate {
    limit: usize,
    active: Mutex<usize>,
    released: Condvar,
}

impl ExecutionGate {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            active: Mutex::new(0),
            released: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> ExecutionSlot {
        let mut active = self.active.lock();
        while *active >= self.limit {
            self.released.wait(&mut active);
        }
        *active += 1;
        ExecutionSlot {
            gate: Arc::clone(self),
        }
    }

    fn release(&self) {
        let mut active = self.active.lock();
        *active = active.saturating_sub(1);
        self.released.notify_one();
    }
}

struct ExecutionSlot {
    gate: Arc<ExecutionGate>,
}

impl Drop for ExecutionSlot {
    fn drop(&mut self) {
        self.gate.release();
    }
}

#[derive(Clone)]
pub struct DelegationScheduler {
    db_path: PathBuf,
    settings: Arc<DaemonSettingsStore>,
    hub: Arc<Hub>,
    registry: Arc<DelegationRegistry>,
    active_children: Arc<Mutex<HashMap<Uuid, ActiveChild>>>,
    notifier: Arc<(Mutex<()>, Condvar)>,
    gate: Arc<ExecutionGate>,
    group_gates: Arc<Mutex<HashMap<Uuid, Arc<ExecutionGate>>>>,
    /// Serializes lifecycle decisions, never provider startup or execution.
    lifecycle: Arc<Mutex<()>>,
}

impl DelegationScheduler {
    pub fn new(
        db_path: PathBuf,
        settings: Arc<DaemonSettingsStore>,
        hub: Arc<Hub>,
        registry: Arc<DelegationRegistry>,
    ) -> Self {
        Self {
            db_path,
            settings,
            hub,
            registry,
            active_children: Arc::new(Mutex::new(HashMap::new())),
            notifier: Arc::new((Mutex::new(()), Condvar::new())),
            gate: Arc::new(ExecutionGate::new(MAX_GLOBAL_CONCURRENT)),
            group_gates: Arc::new(Mutex::new(HashMap::new())),
            lifecycle: Arc::new(Mutex::new(())),
        }
    }

    pub fn registry(&self) -> &DelegationRegistry {
        &self.registry
    }
    pub fn open_connection(&self) -> anyhow::Result<rusqlite::Connection> {
        let store = StateStore::daemon(self.db_path.clone());
        store.open_connection().map_err(Into::into)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn notify_changed(&self) {
        let (lock, changed) = &*self.notifier;
        let _guard = lock.lock();
        changed.notify_all();
    }

    pub fn register_parent(
        &self,
        session_id: &str,
        config: &SessionBridgeConfig,
    ) -> anyhow::Result<PathBuf> {
        let _guard = self.lifecycle.lock();
        self.registry.register_session(session_id, config)
    }

    pub fn cancel_parent(&self, session_id: Uuid, delete: bool) -> anyhow::Result<()> {
        let _guard = self.lifecycle.lock();
        self.registry.unregister_session(&session_id.to_string());
        let connection = self.open_connection()?;
        for group in list_delegation_groups_by_session(&connection, session_id)? {
            self.cancel_locked(group.id, None, &connection)?;
        }
        if delete {
            store::delete_delegations_for_session(&connection, session_id)?;
        }
        self.notify_changed();
        Ok(())
    }

    pub fn shutdown(&self) -> anyhow::Result<()> {
        let connection = self.open_connection()?;
        let mut statement =
            connection.prepare("SELECT DISTINCT parent_session_id FROM delegation_groups")?;
        let sessions = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for session in sessions {
            self.cancel_parent(Uuid::parse_str(&session)?, false)?;
        }
        Ok(())
    }

    pub fn delegate_batch_authorized(
        &self,
        config: &SessionBridgeConfig,
        request: DelegationBatchRequest,
    ) -> anyhow::Result<DelegationBatchResponse> {
        let cwd = config.cwd.clone().unwrap_or(std::env::current_dir()?);
        self.delegate_batch_inner(
            config.parent_session_id,
            config.parent_turn_id.clone(),
            config.parent_runtime_id,
            &cwd,
            config.review_source,
            &config.allowed_targets,
            request,
            Some(config),
        )
    }

    pub fn delegate_batch(
        &self,
        parent_session_id: Uuid,
        parent_turn_id: Option<String>,
        parent_runtime_id: Option<Uuid>,
        cwd: &Path,
        review_source: Option<ReviewDiffSource>,
        allowed_targets: &[ModelReference],
        request: DelegationBatchRequest,
    ) -> anyhow::Result<DelegationBatchResponse> {
        self.delegate_batch_inner(
            parent_session_id,
            parent_turn_id,
            parent_runtime_id,
            cwd,
            review_source,
            allowed_targets,
            request,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn delegate_batch_inner(
        &self,
        parent_session_id: Uuid,
        parent_turn_id: Option<String>,
        parent_runtime_id: Option<Uuid>,
        cwd: &Path,
        review_source: Option<ReviewDiffSource>,
        allowed_targets: &[ModelReference],
        mut request: DelegationBatchRequest,
        authorization: Option<&SessionBridgeConfig>,
    ) -> anyhow::Result<DelegationBatchResponse> {
        anyhow::ensure!(
            authorization.is_none_or(|config| self.registry.is_current(config)),
            "parent delegation capability was revoked"
        );
        if request.tasks.is_empty() {
            bail!("delegation batch must contain at least one task");
        }
        if request.tasks.len() > MAX_TASKS_PER_GROUP {
            bail!("delegation batch cannot exceed {MAX_TASKS_PER_GROUP} tasks");
        }
        let mut keys = HashSet::new();
        for (index, task) in request.tasks.iter().enumerate() {
            anyhow::ensure!(
                !task.task_key.trim().is_empty(),
                "tasks[{index}].task_key must be nonempty. No subtasks were started; retry delegate_batch with the same request_key and a distinct task_key for every subtask."
            );
            anyhow::ensure!(
                keys.insert(&task.task_key),
                "duplicate task_key `{}` at tasks[{index}].task_key: task_key must be unique across the entire batch, even when target_ref differs. Use distinct keys such as books-target-1, books-target-2, and books-target-3. No subtasks were started; retry delegate_batch with the same request_key and unique task_key values.",
                task.task_key,
            );
        }

        let connection = self.open_connection()?;

        // Validate targets against allowed_targets (falling back to the
        // discovered model catalogue when a turn registered with an empty list
        // because the user typed or pasted model names directly).
        let catalog_fallback = if allowed_targets.is_empty() {
            catalog_model_candidates()
        } else {
            Vec::new()
        };
        let mut resolved_tasks = Vec::new();
        for task_spec in &request.tasks {
            let wanted = task_spec.target_ref.trim();
            let targets = allowed_targets
                .iter()
                .filter(|t| {
                    t.reference_id == wanted
                        || t.model_id == wanted
                        || t.display_label.eq_ignore_ascii_case(wanted)
                })
                .collect::<Vec<_>>();
            let target = allowed_targets
                .iter()
                .find(|target| target.reference_id == wanted)
                .cloned()
                .or_else(|| (targets.len() == 1).then(|| targets[0].clone()))
                .or_else(|| {
                    waku_protocol::composer::resolve_model_references(
                        &waku_protocol::composer::model_reference_text(wanted),
                        &catalog_fallback,
                        &[],
                    )
                    .ok()?
                    .into_iter()
                    .next()
                })
                .ok_or_else(|| {
                    anyhow!(
                        "delegation target `{}` is not in the allowed models for this turn",
                        task_spec.target_ref
                    )
                })?;
            resolved_tasks.push((task_spec.clone(), target));
        }
        let request_hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                &parent_turn_id,
                review_source,
                &resolved_tasks
            ))?)
        );
        // A missing key falls back to the batch's own content: retrying the
        // same tasks in the same turn then dedupes exactly as a stable,
        // model-chosen key would.
        if request.request_key.trim().is_empty() {
            request.request_key = format!("auto-{}", &request_hash[..16]);
        }
        let existing_response =
            |existing: DelegationGroupRow| -> anyhow::Result<DelegationBatchResponse> {
                anyhow::ensure!(
                    existing.request_hash == request_hash,
                    "request_key was already used with different tasks or a different turn"
                );
                Ok(DelegationBatchResponse {
                    group_id: existing.id,
                    context_id: existing.context_id,
                    task_ids: list_delegated_tasks_by_group(&connection, existing.id)?
                        .into_iter()
                        .map(|task| (task.task_key, task.id))
                        .collect(),
                })
            };
        if let Some(existing) = get_delegation_group_by_request_key(
            &connection,
            parent_session_id,
            &request.request_key,
        )? {
            return existing_response(existing);
        }
        let diff_source = review_source
            .clone()
            .unwrap_or(ReviewDiffSource::Uncommitted);
        let review_context = if review_source.is_none() && !super::context::is_git_workspace(cwd)? {
            CapturedReviewContext {
                id: Uuid::new_v4(),
                cwd: cwd.to_path_buf(),
                source: diff_source,
                base_oid: String::new(),
                target_oid: String::new(),
                numstat: String::new(),
                patch: String::new(),
                complete_context: true,
                snapshot_dir: None,
                created_at: unix_time(),
            }
        } else {
            capture_review_context(cwd, diff_source)
                .context("could not capture immutable review context")?
        };
        let mut cleanup = SnapshotCleanup(review_context.snapshot_dir.clone());
        let _guard = self.lifecycle.lock();
        anyhow::ensure!(
            authorization.is_none_or(|config| self.registry.is_current(config)),
            "parent delegation capability was revoked while capturing context"
        );
        if let Some(existing) = get_delegation_group_by_request_key(
            &connection,
            parent_session_id,
            &request.request_key,
        )? {
            return existing_response(existing);
        }

        let group_id = Uuid::new_v4();
        let group_row = DelegationGroupRow {
            id: group_id,
            parent_session_id,
            parent_turn_id,
            parent_runtime_id: parent_runtime_id.map(|id| id.to_string()),
            request_key: request.request_key,
            request_hash,
            context_id: review_context.id,
            status: DelegationGroupStatus::Running,
            created_at: unix_time(),
            updated_at: unix_time(),
        };

        let tx = rusqlite::Transaction::new_unchecked(
            &connection,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        insert_review_context(&tx, &review_context)?;
        insert_delegation_group(&tx, &group_row)?;

        let mut task_ids = HashMap::new();
        for (task_spec, target) in resolved_tasks {
            let task_id = Uuid::new_v4();
            let child_session_id = Uuid::new_v4();
            let task_row = DelegatedTaskRow {
                id: task_id,
                group_id,
                task_key: task_spec.task_key.clone(),
                child_session_id,
                runtime_id: None,
                target_config: target,
                instructions: task_spec.instructions,
                status: DelegatedTaskStatus::Queued,
                final_conclusion: None,
                error_message: None,
                latest_activity: None,
                started_at: None,
                finished_at: None,
            };
            insert_delegated_task(&tx, &task_row)?;
            task_ids.insert(task_spec.task_key, task_id);
        }
        tx.commit()?;
        cleanup.0 = None;

        // Spawn runner for this delegation group
        let context_id = review_context.id;
        let scheduler = self.clone();
        let spawned = thread::Builder::new()
            .name(format!("waku-delegation-group-{group_id}"))
            .spawn(move || {
                scheduler.run_group(group_id, review_context);
            })
            .context("could not spawn delegation group runner");
        if let Err(error) = spawned {
            for task in task_ids.values() {
                let _ = finish_delegated_task(
                    &connection,
                    *task,
                    None,
                    DelegatedTaskStatus::Failed,
                    None,
                    Some(&error.to_string()),
                );
            }
            self.notify_changed();
            return Err(error);
        }

        Ok(DelegationBatchResponse {
            group_id,
            task_ids,
            context_id,
        })
    }

    pub fn wait_delegates(
        &self,
        group_id: Uuid,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<WaitDelegatesResponse> {
        let max_wait = Duration::from_millis(timeout_ms.unwrap_or(25_000).min(25_000));
        let deadline = Instant::now() + max_wait;

        let (lock, cvar) = &*self.notifier;
        let mut guard = lock.lock();

        loop {
            let connection = self.open_connection()?;
            let snapshot = connection.unchecked_transaction()?;
            let group = get_delegation_group(&snapshot, group_id)?
                .ok_or_else(|| anyhow!("delegation group {group_id} not found"))?;
            let task_rows = list_delegated_tasks_by_group(&snapshot, group_id)?;
            let status = store::projected_group_status(group.status, &task_rows);
            snapshot.commit()?;
            let all_terminal =
                !task_rows.is_empty() && task_rows.iter().all(|t| t.status.is_terminal());

            if all_terminal || Instant::now() >= deadline {
                let tasks = task_rows.into_iter().map(|t| t.to_summary()).collect();
                return Ok(WaitDelegatesResponse {
                    group_id,
                    status,
                    tasks,
                });
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            cvar.wait_for(&mut guard, remaining);
        }
    }

    pub fn cancel(&self, group_id: Uuid, task_id: Option<Uuid>) -> anyhow::Result<()> {
        let _guard = self.lifecycle.lock();
        let connection = self.open_connection()?;
        self.cancel_locked(group_id, task_id, &connection)?;
        self.notify_changed();
        Ok(())
    }

    fn cancel_locked(
        &self,
        group_id: Uuid,
        task_id: Option<Uuid>,
        connection: &rusqlite::Connection,
    ) -> anyhow::Result<()> {
        let tasks = list_delegated_tasks_by_group(connection, group_id)?;
        anyhow::ensure!(
            task_id.is_none_or(|id| tasks.iter().any(|task| task.id == id)),
            "task does not belong to this group"
        );
        for task in tasks
            .iter()
            .filter(|task| task_id.is_none_or(|id| id == task.id))
        {
            if task.status.is_terminal() {
                continue;
            }
            finish_delegated_task(
                connection,
                task.id,
                task.runtime_id,
                DelegatedTaskStatus::Cancelled,
                None,
                Some("delegation cancelled by user"),
            )?;
            if let Some(child) = self
                .active_children
                .lock()
                .get(&task.id)
                .filter(|child| Some(child.runtime_id) == task.runtime_id)
            {
                child.cancelled.store(true, Ordering::Release);
                child.handle.cancel();
            }
        }
        if task_id.is_none() && tasks.iter().any(|task| !task.status.is_terminal()) {
            update_delegation_group_status(connection, group_id, DelegationGroupStatus::Cancelled)?;
        }
        Ok(())
    }

    pub fn retry_task(&self, group_id: Uuid, task_id: Uuid) -> anyhow::Result<()> {
        let _guard = self.lifecycle.lock();
        let connection = self.open_connection()?;
        let group = get_delegation_group(&connection, group_id)?
            .ok_or_else(|| anyhow!("delegation group {group_id} not found"))?;
        let review_context = get_review_context(&connection, group.context_id)?
            .ok_or_else(|| anyhow!("review context {} not found", group.context_id))?;
        anyhow::ensure!(
            review_context
                .snapshot_dir
                .as_ref()
                .is_none_or(|path| path.is_dir()),
            "the saved review snapshot is no longer available"
        );
        let runtime_id = Uuid::new_v4();
        store::reset_delegated_task(&connection, group_id, task_id, runtime_id)?;
        let scheduler = self.clone();
        let spawned = thread::Builder::new()
            .name(format!("waku-retry-task-{task_id}"))
            .spawn(move || {
                scheduler.run_single_task(task_id, Some(runtime_id), &review_context);
            });
        if let Err(error) = spawned {
            finish_delegated_task(
                &connection,
                task_id,
                Some(runtime_id),
                DelegatedTaskStatus::Failed,
                None,
                Some(&format!("could not start retry: {error}")),
            )?;
            self.notify_changed();
            return Err(error.into());
        }
        self.notify_changed();
        Ok(())
    }

    pub fn get_group_info(&self, group_id: Uuid) -> anyhow::Result<Option<DelegationGroupInfo>> {
        let connection = self.open_connection()?;
        let snapshot = connection.unchecked_transaction()?;
        let Some(group) = get_delegation_group(&snapshot, group_id)? else {
            return Ok(None);
        };
        let review_context = get_review_context(&snapshot, group.context_id)?;
        let task_rows = list_delegated_tasks_by_group(&snapshot, group_id)?;
        let status = store::projected_group_status(group.status, &task_rows);
        let tasks = task_rows.into_iter().map(|t| t.to_summary()).collect();
        let review_source = review_context.as_ref().map(|context| context.source);
        let patch_summary = review_context.map(|c| c.numstat);

        Ok(Some(DelegationGroupInfo {
            id: group.id,
            parent_session_id: group.parent_session_id,
            parent_turn_id: group.parent_turn_id,
            status,
            review_source,
            patch_summary,
            tasks,
            created_at: group.created_at,
            updated_at: group.updated_at,
        }))
    }

    pub fn list_session_groups(
        &self,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<DelegationGroupInfo>> {
        let connection = self.open_connection()?;
        let snapshot = connection.unchecked_transaction()?;
        let groups = list_delegation_groups_by_session(&snapshot, session_id)?;
        let mut results = Vec::new();
        for group in groups {
            let task_rows = list_delegated_tasks_by_group(&snapshot, group.id)?;
            let status = store::projected_group_status(group.status, &task_rows);
            let tasks = task_rows.into_iter().map(|t| t.to_summary()).collect();
            results.push(DelegationGroupInfo {
                id: group.id,
                parent_session_id: group.parent_session_id,
                parent_turn_id: group.parent_turn_id,
                status,
                review_source: None,
                patch_summary: None,
                tasks,
                created_at: group.created_at,
                updated_at: group.updated_at,
            });
        }
        Ok(results)
    }

    fn run_group(&self, group_id: Uuid, context: CapturedReviewContext) {
        let connection = match self.open_connection() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("failed to open connection for group {group_id}: {e}");
                return;
            }
        };

        let tasks = match list_delegated_tasks_by_group(&connection, group_id) {
            Ok(tasks) => tasks,
            Err(e) => {
                eprintln!("failed to list tasks for group {group_id}: {e}");
                return;
            }
        };

        let mut tasks = tasks;
        tasks.retain(|task| !task.status.is_terminal());

        // The per-group and global gates admit a queued task as soon as any
        // child frees a slot, without waiting for its running siblings.
        let context = Arc::new(context);
        let mut handles = Vec::with_capacity(tasks.len());
        for task in tasks {
            let scheduler = self.clone();
            let context_clone = Arc::clone(&context);
            let task_id = task.id;
            let expected_runtime = task.runtime_id;
            match thread::Builder::new()
                .name(format!("waku-task-{task_id}"))
                .spawn(move || {
                    scheduler.run_single_task(task_id, expected_runtime, &context_clone);
                }) {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    let _ = finish_delegated_task(
                        &connection,
                        task_id,
                        expected_runtime,
                        DelegatedTaskStatus::Failed,
                        None,
                        Some(&format!("could not start task: {error}")),
                    );
                    self.notify_changed();
                }
            }
        }
        for handle in handles {
            let _ = handle.join();
        }

        // Each terminal task update commits its group projection atomically.
        self.notify_changed();
    }

    fn run_single_task(
        &self,
        task_id: Uuid,
        expected_runtime: Option<Uuid>,
        context: &CapturedReviewContext,
    ) {
        let connection = match self.open_connection() {
            Ok(conn) => conn,
            Err(_) => return,
        };

        let task = match store::get_delegated_task(&connection, task_id) {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(_) => return,
        };
        if task.status != DelegatedTaskStatus::Queued || task.runtime_id != expected_runtime {
            // Cancelled while queued: nothing to run.
            return;
        }

        let target = &task.target_config;
        let runtime_id = expected_runtime.unwrap_or_else(Uuid::new_v4);
        let child_session_id = task.child_session_id;

        let group_gate = self
            .group_gates
            .lock()
            .entry(task.group_id)
            .or_insert_with(|| Arc::new(ExecutionGate::new(MAX_CONCURRENT_PER_GROUP)))
            .clone();
        let _group_slot = group_gate.acquire();
        let _slot = self.gate.acquire();
        let started = Instant::now();
        {
            // Cancellation reads and finishes the same generation while it
            // owns this lock; a queued claim must not race that read.
            let _guard = self.lifecycle.lock();
            match store::begin_delegated_attempt(&connection, task_id, expected_runtime, runtime_id)
            {
                Ok(true) => {}
                _ => return,
            }
        }

        // Resolve executable binary
        let binary = match self.resolve_binary(target.provider) {
            Ok(b) => b,
            Err(e) => {
                let _ = finish_delegated_task(
                    &connection,
                    task_id,
                    Some(runtime_id),
                    DelegatedTaskStatus::Failed,
                    None,
                    Some(&format!(
                        "could not resolve binary for {}: {e}",
                        target.provider.display_name()
                    )),
                );
                self.notify_changed();
                return;
            }
        };

        let cwd = match &context.snapshot_dir {
            Some(dir) => dir.clone(),
            None => context.cwd.clone(),
        };

        let options = DriverStartOptions {
            binary,
            cwd,
            mode: RuntimeMode::Ask,
            execution_policy: ExecutionPolicy::ReviewReadOnly,
            model: Some(target.model_id.clone()),
            reasoning_effort: target.reasoning_effort.clone(),
            service_tier: target.service_tier.clone(),
            context_window: target.context_window.clone(),
            agent_preset: target.agent_preset.clone(),
            provider_cursor: None,
            delegation_session_path: None,
            delegation_registry_dir: None,
        };

        // Wait for a daemon-wide slot before spawning a provider process, and
        // re-check the task after waiting: it may have been cancelled meanwhile.
        let (wake, _wake_events) = smol::channel::bounded(1);
        let (event_sender, event_receiver) = driver::event_channel(wake);

        self.hub.begin_runtime(child_session_id, runtime_id);
        let event_sink = self.hub.event_sink(child_session_id, runtime_id);

        let handle = match driver::start_local(target.provider, options, event_sender) {
            Ok(h) => h,
            Err(e) => {
                let _ = finish_delegated_task(
                    &connection,
                    task_id,
                    Some(runtime_id),
                    DelegatedTaskStatus::Failed,
                    None,
                    Some(&format!("failed to start child driver: {e}")),
                );
                self.hub.end_runtime(child_session_id, Some(runtime_id));
                self.notify_changed();
                return;
            }
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        // Startup may block. Claim the active handle and send the prompt only
        // after rechecking cancellation under the lifecycle lock.
        {
            let _guard = self.lifecycle.lock();
            if !matches!(store::get_delegated_task(&connection, task_id),
                Ok(Some(current)) if current.runtime_id == Some(runtime_id)
                    && current.status == DelegatedTaskStatus::Starting)
            {
                handle.cancel();
                self.hub.end_runtime(child_session_id, Some(runtime_id));
                return;
            }
            let mut children = self.active_children.lock();
            children.insert(
                task_id,
                ActiveChild {
                    handle: handle.clone(),
                    runtime_id,
                    cancelled: cancelled.clone(),
                },
            );
            let _ = update_delegated_task_progress(
                &connection,
                task_id,
                Some(runtime_id),
                DelegatedTaskStatus::Running,
                Some("starting review"),
            );

            // Send instructions prompt. A configured sub-agent's standing
            // instructions lead, so the parent's per-task text is read in
            // the role the user gave that agent.
            let role = target
                .agent_instructions
                .as_deref()
                .map(str::trim)
                .filter(|instructions| !instructions.is_empty())
                .map(|instructions| {
                    format!(
                        "You are the \"{}\" agent. Follow these standing instructions:\n{instructions}\n\n",
                        target.display_label
                    )
                })
                .unwrap_or_default();
            let prompt = if context.patch.trim().is_empty() {
                format!(
                    "{role}You are executing an independent subtask.\n\n\
                 Task:\n{}\n\n\
                 Please provide your thorough findings, evaluation, and final conclusion.",
                    task.instructions
                )
            } else {
                format!(
                    "{role}You are an expert code reviewer performing an independent code review.\n\
                 The code diff is in `.waku-review/patch.diff`.\n\
                 Inspect the changes and source files in this workspace as needed.\n\n\
                 Task:\n{}\n\n\
                 Please provide your thorough findings, evaluation, and final conclusion.",
                    task.instructions
                )
            };
            handle.prompt(prompt);
        }

        // Consume child event stream
        let image_blobs = StateStore::daemon(self.db_path.clone()).blobs();
        let mut full_text = String::new();
        let mut sequence = 0u64;
        let mut final_status = DelegatedTaskStatus::Completed;
        let mut error_msg = None;

        loop {
            if cancelled.load(Ordering::Acquire) {
                final_status = DelegatedTaskStatus::Cancelled;
                break;
            }
            let remaining = match TASK_TIMEOUT.checked_sub(started.elapsed()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    handle.cancel();
                    final_status = DelegatedTaskStatus::TimedOut;
                    error_msg = Some(format!(
                        "review subtask exceeded the {} minute limit",
                        TASK_TIMEOUT.as_secs() / 60
                    ));
                    break;
                }
            };
            let mut event = match event_receiver.recv_timeout(remaining.min(Duration::from_millis(200)))
            {
                Ok(event) => event,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    // The driver channel closed without a terminal event.
                    final_status = DelegatedTaskStatus::Interrupted;
                    error_msg = Some("provider runtime ended without finishing".into());
                    break;
                }
            };
            if let DriverEvent::RichActivity(activity) = &mut event {
                crate::image_generation::externalize(activity, &image_blobs);
            }
            sequence += 1;
            let wire = event_to_wire(event.clone())
                .unwrap_or_else(|_| WireDriverEvent::new("event", serde_json::Value::Null));
            let Ok(wire_json) = serde_json::to_string(&wire) else {
                continue;
            };
            match store::append_delegation_event_for_runtime(
                &connection,
                task_id,
                runtime_id,
                sequence,
                &wire_json,
            ) {
                Ok(true) => {}
                Ok(false) => {
                    // The row was cancelled, deleted, or replaced by a newer
                    // attempt. Stop this provider even if cancellation has
                    // not yet signalled its in-memory handle.
                    final_status = DelegatedTaskStatus::Cancelled;
                    break;
                }
                Err(error) => {
                    final_status = DelegatedTaskStatus::Failed;
                    error_msg = Some(format!("could not persist delegated output: {error}"));
                    handle.cancel();
                    break;
                }
            }
            let _ = event_sink.send(wire);

            match event {
                DriverEvent::TextDelta(delta) => {
                    full_text.push_str(&delta);
                }
                DriverEvent::RichActivity(activity) => {
                    let _ = update_delegated_task_progress(
                        &connection,
                        task_id,
                        Some(runtime_id),
                        DelegatedTaskStatus::Running,
                        Some(&activity.title),
                    );
                }
                DriverEvent::TurnFinished { success, summary } => {
                    if !success {
                        final_status = DelegatedTaskStatus::Failed;
                        error_msg =
                            summary.or_else(|| Some("review subtask ended unsuccessfully".into()));
                    }
                    break;
                }
                DriverEvent::Error(err) => {
                    final_status = DelegatedTaskStatus::Failed;
                    error_msg = Some(err);
                    break;
                }
                DriverEvent::ProcessExited => {
                    final_status = DelegatedTaskStatus::Interrupted;
                    error_msg = Some("provider runtime ended without finishing".into());
                    break;
                }
                DriverEvent::UserInputRequested { .. } | DriverEvent::Permission { .. } => {
                    final_status = DelegatedTaskStatus::Failed;
                    error_msg =
                        Some("the provider requested interaction in an unattended subtask".into());
                    break;
                }
                _ => {}
            }
        }

        // Task finished
        if final_status != DelegatedTaskStatus::Completed {
            handle.cancel();
        }
        {
            let mut children = self.active_children.lock();
            if children
                .get(&task_id)
                .is_some_and(|child| child.runtime_id == runtime_id)
            {
                children.remove(&task_id);
            }
        }
        self.hub.end_runtime(child_session_id, Some(runtime_id));

        let conclusion = if !full_text.trim().is_empty() {
            Some(full_text.trim().to_owned())
        } else {
            None
        };

        let _ = finish_delegated_task(
            &connection,
            task_id,
            Some(runtime_id),
            final_status,
            conclusion.as_deref(),
            error_msg.as_deref(),
        );

        self.notify_changed();
    }

    fn resolve_binary(&self, provider: ProviderKind) -> anyhow::Result<PathBuf> {
        let settings = self.settings.get();
        let override_value = settings
            .provider_binary_overrides
            .get(&provider)
            .map(String::as_str);
        crate::model::provider_probe(provider, override_value)
            .path
            .ok_or_else(|| anyhow!("provider {} is not installed", provider.display_name()))
    }
}

pub fn catalog_model_candidates() -> Vec<waku_protocol::composer::ModelCandidate> {
    let mut models = Vec::new();
    for provider in ProviderKind::ALL {
        if !provider.capabilities().delegation.review_subtask {
            continue;
        }
        let list = crate::model_catalog::cached_models(provider)
            .unwrap_or_else(|| crate::model_catalog::fallback_models(provider));
        for model in list {
            models.push((provider, model));
        }
    }
    waku_protocol::composer::model_candidates(&models)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use waku_protocol::delegation::DelegationTaskSpec;

    struct Fixture {
        scheduler: DelegationScheduler,
        directory: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("waku-delegation-runner-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let binary = directory.join("fixture-claude");
            let log = directory
                .join("launches")
                .display()
                .to_string()
                .replace('\'', "'\\''");
            let script = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      printf 'launch\n' >> '@LOG@'
      printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"fixture conclusion"}]}}'
      case "$line" in
        *WAIT_FOR_CANCEL*) ;;
        *) printf '%s\n' '{"type":"result","is_error":false,"stop_reason":"end_turn","num_turns":1}' ;;
      esac ;;
    *'"subtype":"interrupt"'*)
      printf 'cancel\n' >> '@LOG@.cancelled'
      printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"late output"}]}}' '{"type":"result","is_error":false,"stop_reason":"end_turn","num_turns":1}' ;;
  esac
done
"#;
            std::fs::write(&binary, script.replace("@LOG@", &log)).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let settings = DaemonSettingsStore::open(directory.join("settings.json")).unwrap();
            let mut config = settings.get();
            config
                .provider_binary_overrides
                .insert(ProviderKind::Claude, binary.display().to_string());
            settings.replace(config).unwrap();
            let scheduler = DelegationScheduler::new(
                directory.join("app.db"),
                Arc::new(settings),
                Arc::new(Hub::default()),
                Arc::new(DelegationRegistry::new().unwrap()),
            );
            Self {
                scheduler,
                directory,
            }
        }

        fn target(&self) -> ModelReference {
            ModelReference::new("target", ProviderKind::Claude, "fixture", "Fixture")
        }

        fn request(&self, instructions: &str) -> DelegationBatchRequest {
            DelegationBatchRequest {
                request_key: "review".into(),
                tasks: vec![DelegationTaskSpec {
                    task_key: "review".into(),
                    target_ref: "target".into(),
                    instructions: instructions.into(),
                }],
            }
        }

        fn start(&self, instructions: &str) -> DelegationBatchResponse {
            self.scheduler
                .delegate_batch(
                    Uuid::new_v4(),
                    None,
                    None,
                    &self.directory,
                    None,
                    &[self.target()],
                    self.request(instructions),
                )
                .unwrap()
        }

        fn launches(&self) -> usize {
            std::fs::read_to_string(self.directory.join("launches"))
                .unwrap_or_default()
                .lines()
                .count()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.scheduler.shutdown().unwrap();
            until(|| self.scheduler.active_children.lock().is_empty());
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "fixture did not settle");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn retry_starts_a_new_attempt_and_wait_returns_its_terminal_projection() {
        let fixture = Fixture::new();
        let batch = fixture.start("review independently");
        let first = fixture
            .scheduler
            .wait_delegates(batch.group_id, Some(5000))
            .unwrap();
        assert_eq!(first.status, DelegationGroupStatus::Completed);
        assert_eq!(
            first.tasks[0].final_conclusion.as_deref(),
            Some("fixture conclusion")
        );
        fixture
            .scheduler
            .retry_task(batch.group_id, first.tasks[0].task_id)
            .unwrap();
        let next = fixture
            .scheduler
            .wait_delegates(batch.group_id, Some(5000))
            .unwrap();
        assert_eq!(next.status, DelegationGroupStatus::Completed);
        assert_ne!(next.tasks[0].attempt_id, first.tasks[0].attempt_id);
        assert_eq!(fixture.launches(), 2);
        let connection = fixture.scheduler.open_connection().unwrap();
        let rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM delegation_events WHERE task_id = ?1 AND sequence = 0",
                [first.tasks[0].task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "the original attempt remains in persisted history");
        // Also repair the projection of a legacy row left stale by the old runner.
        update_delegation_group_status(&connection, batch.group_id, DelegationGroupStatus::Running)
            .unwrap();
        assert_eq!(
            fixture
                .scheduler
                .wait_delegates(batch.group_id, Some(0))
                .unwrap()
                .status,
            DelegationGroupStatus::Completed
        );
    }

    #[test]
    fn cancel_keeps_its_terminal_status_when_the_provider_completes_late() {
        let fixture = Fixture::new();
        let batch = fixture.start("WAIT_FOR_CANCEL");
        until(|| fixture.launches() == 1);
        let task = *batch.task_ids.get("review").unwrap();
        fixture
            .scheduler
            .cancel(batch.group_id, Some(task))
            .unwrap();
        until(|| fixture.scheduler.active_children.lock().is_empty());
        let settled = fixture
            .scheduler
            .wait_delegates(batch.group_id, Some(0))
            .unwrap();
        assert_eq!(settled.status, DelegationGroupStatus::Cancelled);
        assert_eq!(settled.tasks[0].status, DelegatedTaskStatus::Cancelled);
        assert!(settled.tasks[0].final_conclusion.is_none());
    }

    #[test]
    fn a_freed_group_slot_starts_a_queued_sibling_while_another_child_is_running() {
        let fixture = Fixture::new();
        let mut request = fixture.request("WAIT_FOR_CANCEL");
        for index in 1..MAX_TASKS_PER_GROUP {
            let mut task = request.tasks[0].clone();
            task.task_key = format!("sibling-{index}");
            request.tasks.push(task);
        }
        let batch = fixture
            .scheduler
            .delegate_batch(
                Uuid::new_v4(),
                None,
                None,
                &fixture.directory,
                None,
                &[fixture.target()],
                request,
            )
            .unwrap();
        until(|| fixture.launches() == MAX_CONCURRENT_PER_GROUP);

        let connection = fixture.scheduler.open_connection().unwrap();
        let tasks = list_delegated_tasks_by_group(&connection, batch.group_id).unwrap();
        let running = tasks
            .iter()
            .filter(|task| task.status == DelegatedTaskStatus::Running)
            .collect::<Vec<_>>();
        let queued = tasks
            .iter()
            .filter(|task| task.status == DelegatedTaskStatus::Queued)
            .map(|task| task.id)
            .collect::<HashSet<_>>();
        assert_eq!(running.len(), MAX_CONCURRENT_PER_GROUP);
        assert_eq!(queued.len(), MAX_TASKS_PER_GROUP - MAX_CONCURRENT_PER_GROUP);

        // Only one child settles; its running sibling must not hold up the queue.
        fixture
            .scheduler
            .cancel(batch.group_id, Some(running[0].id))
            .unwrap();
        until(|| {
            list_delegated_tasks_by_group(&connection, batch.group_id)
                .unwrap()
                .iter()
                .any(|task| {
                    queued.contains(&task.id) && task.status == DelegatedTaskStatus::Running
                })
        });
        let current = list_delegated_tasks_by_group(&connection, batch.group_id).unwrap();
        assert!(current.iter().any(|task| {
            task.id == running[1].id && task.status == DelegatedTaskStatus::Running
        }));
        assert_eq!(
            current
                .iter()
                .filter(|task| task.status == DelegatedTaskStatus::Running)
                .count(),
            MAX_CONCURRENT_PER_GROUP
        );
        assert_eq!(
            *fixture.scheduler.gate.active.lock(),
            MAX_CONCURRENT_PER_GROUP
        );
    }

    #[test]
    fn a_rejected_event_cancels_the_provider_before_the_cancel_flag_is_set() {
        let fixture = Fixture::new();
        let batch = fixture.start("WAIT_FOR_CANCEL");
        until(|| fixture.launches() == 1);
        let task_id = batch.task_ids["review"];
        // Keep a lease so dropping the runner cannot hide a missing cancel.
        let (handle, runtime_id, cancelled) = {
            let children = fixture.scheduler.active_children.lock();
            let child = children.get(&task_id).unwrap();
            (
                child.handle.clone(),
                child.runtime_id,
                child.cancelled.clone(),
            )
        };
        let connection = fixture.scheduler.open_connection().unwrap();
        // Pause cancellation after its database commit, before it signals the child.
        assert!(
            finish_delegated_task(
                &connection,
                task_id,
                Some(runtime_id),
                DelegatedTaskStatus::Cancelled,
                None,
                Some("cancelled before signalling the provider"),
            )
            .unwrap()
        );
        // Make the fixture emit another event against the now-terminal row.
        handle.prompt("WAIT_FOR_CANCEL late output".into());
        until(|| fixture.scheduler.active_children.lock().is_empty());
        until(|| fixture.directory.join("launches.cancelled").exists());
        assert!(!cancelled.load(Ordering::Acquire));
        let settled = fixture
            .scheduler
            .wait_delegates(batch.group_id, Some(0))
            .unwrap();
        assert_eq!(settled.tasks[0].status, DelegatedTaskStatus::Cancelled);
        assert!(settled.tasks[0].final_conclusion.is_none());
    }

    #[test]
    fn cancellation_cannot_race_the_claim_of_a_queued_generation() {
        let fixture = Fixture::new();
        let connection = fixture.scheduler.open_connection().unwrap();
        let context = CapturedReviewContext {
            id: Uuid::new_v4(),
            cwd: fixture.directory.clone(),
            source: ReviewDiffSource::Uncommitted,
            base_oid: String::new(),
            target_oid: String::new(),
            numstat: String::new(),
            patch: String::new(),
            complete_context: true,
            snapshot_dir: None,
            created_at: 0,
        };
        let group = Uuid::new_v4();
        let task = Uuid::new_v4();
        insert_review_context(&connection, &context).unwrap();
        insert_delegation_group(
            &connection,
            &DelegationGroupRow {
                id: group,
                parent_session_id: Uuid::new_v4(),
                parent_turn_id: None,
                parent_runtime_id: None,
                request_key: "claim".into(),
                request_hash: "hash".into(),
                context_id: context.id,
                status: DelegationGroupStatus::Running,
                created_at: 0,
                updated_at: 0,
            },
        )
        .unwrap();
        insert_delegated_task(
            &connection,
            &DelegatedTaskRow {
                id: task,
                group_id: group,
                task_key: "task".into(),
                child_session_id: Uuid::new_v4(),
                runtime_id: None,
                target_config: fixture.target(),
                instructions: "must never start".into(),
                status: DelegatedTaskStatus::Queued,
                final_conclusion: None,
                error_message: None,
                latest_activity: None,
                started_at: None,
                finished_at: None,
            },
        )
        .unwrap();
        let cancelling = fixture.scheduler.lifecycle.lock();
        let scheduler = fixture.scheduler.clone();
        let runner = thread::spawn(move || scheduler.run_single_task(task, None, &context));
        until(|| *fixture.scheduler.gate.active.lock() > 0);
        // cancel_locked reads this generation while holding the same lock.
        // A provider runner must not change it between that read and finish.
        assert_eq!(
            store::get_delegated_task(&connection, task)
                .unwrap()
                .unwrap()
                .status,
            DelegatedTaskStatus::Queued
        );
        finish_delegated_task(
            &connection,
            task,
            None,
            DelegatedTaskStatus::Cancelled,
            None,
            None,
        )
        .unwrap();
        drop(cancelling);
        runner.join().unwrap();
        assert_eq!(fixture.launches(), 0);
        assert_eq!(
            fixture
                .scheduler
                .wait_delegates(group, Some(0))
                .unwrap()
                .tasks[0]
                .status,
            DelegatedTaskStatus::Cancelled
        );
    }

    #[test]
    fn stopping_a_parent_cancels_queued_siblings_and_revokes_future_batches() {
        let fixture = Fixture::new();
        let parent = Uuid::new_v4();
        let config = SessionBridgeConfig {
            endpoint: "fixture".into(),
            token: "capability".into(),
            parent_session_id: parent,
            parent_turn_id: Some("turn".into()),
            parent_runtime_id: None,
            cwd: Some(fixture.directory.clone()),
            allowed_targets: vec![fixture.target()],
            review_source: None,
        };
        fixture
            .scheduler
            .register_parent(&parent.to_string(), &config)
            .unwrap();
        let mut request = fixture.request("WAIT_FOR_CANCEL");
        for index in 1..4 {
            let mut task = request.tasks[0].clone();
            task.task_key = format!("sibling-{index}");
            request.tasks.push(task);
        }
        let batch = fixture
            .scheduler
            .delegate_batch_authorized(&config, request.clone())
            .unwrap();
        until(|| fixture.launches() > 0);
        fixture.scheduler.cancel_parent(parent, false).unwrap();
        let started_at_stop = fixture.launches();
        until(|| fixture.scheduler.active_children.lock().is_empty());
        assert_eq!(fixture.launches(), started_at_stop);
        assert!(
            fixture
                .scheduler
                .wait_delegates(batch.group_id, Some(0))
                .unwrap()
                .tasks
                .iter()
                .all(|task| task.status == DelegatedTaskStatus::Cancelled)
        );
        request.request_key = "late request".into();
        assert!(
            fixture
                .scheduler
                .delegate_batch_authorized(&config, request)
                .is_err()
        );
        fixture.scheduler.cancel_parent(parent, true).unwrap();
        assert!(
            fixture
                .scheduler
                .list_session_groups(parent)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_git_snapshot_error_does_not_fall_back_to_the_mutable_worktree() {
        let fixture = Fixture::new();
        let repo = fixture.directory.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        git(&["init", "-b", "main"]);
        std::fs::write(repo.join("source.rs"), "source").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "baseline",
        ]);
        std::fs::write(repo.join(".waku-review"), "reserved path is occupied").unwrap();
        let parent = Uuid::new_v4();
        let result = fixture.scheduler.delegate_batch(
            parent,
            None,
            None,
            &repo,
            None,
            &[fixture.target()],
            fixture.request("review the diff"),
        );
        assert!(result.is_err());
        assert_eq!(fixture.launches(), 0);
        assert!(
            fixture
                .scheduler
                .list_session_groups(parent)
                .unwrap()
                .is_empty()
        );
    }
}
