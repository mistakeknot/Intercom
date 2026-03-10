//! Per-group serialization queue with global concurrency cap.
//!
//! Port of `src/group-queue.ts`. Ensures only one container runs per group
//! at a time, with a global limit on total concurrent containers.
//!
//! Key semantics:
//! - Tasks drain before messages (priority ordering)
//! - Follow-up messages piped to active containers via IPC `input/` directory
//! - Exponential retry backoff on message processing failure
//! - Graceful shutdown: containers are detached (not killed)

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use futures::FutureExt;
use intercom_core::ContainerOutput;
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

const MAX_RETRIES: u32 = 5;
const BASE_RETRY_MS: u64 = 5000;

/// Callback for processing messages for a group. Returns true on success.
pub type ProcessMessagesFn =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Callback for running a queued task.
pub type TaskFn = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// A queued task waiting for execution.
struct QueuedTask {
    id: String,
    #[allow(dead_code)]
    group_jid: String,
    task_fn: TaskFn,
}

/// A warm (long-running) container that persists between messages.
/// Held inside GroupState — single state owner prevents dual-state-machine bugs.
pub struct PoolContainer {
    /// Name of the running Docker container.
    pub container_name: String,
    /// The foreground Docker child process (for crash detection via wait()).
    pub child: Child,
    /// When this container was spawned.
    pub started_at: Instant,
    /// Last time a message was sent to this container (for idle reaping).
    pub last_activity: Instant,
    /// Whether the container is actively processing a query (can't respond to pings).
    pub active_delivery: bool,
    /// Background task monitoring child process exit.
    pub exit_monitor: JoinHandle<()>,
    /// Background task reading UDS output frames.
    pub uds_listener: JoinHandle<()>,
    /// Channel for sending output frames to per-message delivery handlers.
    pub delivery_tx: tokio::sync::mpsc::Sender<ContainerOutput>,
}

/// Per-group state tracked by the queue.
struct GroupState {
    active: bool,
    idle_waiting: bool,
    is_task_container: bool,
    pending_messages: bool,
    pending_tasks: VecDeque<QueuedTask>,
    container_name: Option<String>,
    group_folder: Option<String>,
    retry_count: u32,
    /// Warm container for this group (None = cold, must spawn fresh).
    pool_container: Option<PoolContainer>,
}

impl Default for GroupState {
    fn default() -> Self {
        Self {
            active: false,
            idle_waiting: false,
            is_task_container: false,
            pending_messages: false,
            pending_tasks: VecDeque::new(),
            container_name: None,
            group_folder: None,
            retry_count: 0,
            pool_container: None,
        }
    }
}

/// Shared inner state behind a mutex.
struct Inner {
    groups: HashMap<String, GroupState>,
    active_count: usize,
    max_concurrent: usize,
    waiting_groups: VecDeque<String>,
    process_messages_fn: Option<ProcessMessagesFn>,
    shutting_down: bool,
    data_dir: PathBuf,
}

impl Inner {
    fn get_or_insert(&mut self, jid: &str) -> &mut GroupState {
        self.groups
            .entry(jid.to_string())
            .or_insert_with(GroupState::default)
    }

    fn reset_group(&mut self, jid: &str) {
        if let Some(state) = self.groups.get_mut(jid) {
            state.active = false;
            state.is_task_container = false;
            state.container_name = None;
            // Note: group_folder is intentionally NOT cleared — it's needed
            // for pool container operations even when no container is active.
            // pool_container is NOT cleared here either — warm containers
            // persist across message processing cycles.
        }
        self.active_count = self.active_count.saturating_sub(1);
    }
}

/// Group queue managing per-group serialization and global concurrency.
pub struct GroupQueue {
    inner: Arc<Mutex<Inner>>,
}

impl GroupQueue {
    pub fn new(max_concurrent: usize, data_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                groups: HashMap::new(),
                active_count: 0,
                max_concurrent,
                waiting_groups: VecDeque::new(),
                process_messages_fn: None,
                shutting_down: false,
                data_dir,
            })),
        }
    }

    /// Set the callback invoked to process messages for a group.
    pub async fn set_process_messages_fn(&self, f: ProcessMessagesFn) {
        self.inner.lock().await.process_messages_fn = Some(f);
    }

    /// Enqueue a message check for a group.
    pub async fn enqueue_message_check(&self, group_jid: &str) {
        let should_spawn = {
            let mut inner = self.inner.lock().await;
            if inner.shutting_down {
                return;
            }

            let state = inner.get_or_insert(group_jid);

            if state.active {
                state.pending_messages = true;
                debug!(group_jid, "container active, message queued");
                return;
            }

            if inner.active_count >= inner.max_concurrent {
                let state = inner.get_or_insert(group_jid);
                state.pending_messages = true;
                let jid = group_jid.to_string();
                if !inner.waiting_groups.contains(&jid) {
                    inner.waiting_groups.push_back(jid);
                }
                debug!(
                    group_jid,
                    active_count = inner.active_count,
                    "at concurrency limit, message queued"
                );
                return;
            }

            // Can run immediately
            let state = inner.get_or_insert(group_jid);
            state.active = true;
            state.idle_waiting = false;
            state.is_task_container = false;
            state.pending_messages = false;
            inner.active_count += 1;
            true
        };

        if should_spawn {
            let queue = self.inner.clone();
            let jid = group_jid.to_string();
            tokio::spawn(async move {
                run_for_group(queue, jid).await;
            });
        }
    }

    /// Enqueue a task for a group. Tasks have priority over messages.
    pub async fn enqueue_task(&self, group_jid: &str, task_id: &str, task_fn: TaskFn) {
        let task_to_run = {
            let mut inner = self.inner.lock().await;
            if inner.shutting_down {
                return;
            }

            let data_dir = inner.data_dir.clone();
            let state = inner.get_or_insert(group_jid);

            // Deduplicate
            if state.pending_tasks.iter().any(|t| t.id == task_id) {
                debug!(group_jid, task_id, "task already queued, skipping");
                return;
            }

            if state.active {
                let close_folder = if state.idle_waiting {
                    state.group_folder.clone()
                } else {
                    None
                };
                state.pending_tasks.push_back(QueuedTask {
                    id: task_id.to_string(),
                    group_jid: group_jid.to_string(),
                    task_fn,
                });
                if let Some(ref folder) = close_folder {
                    write_close_sentinel(&data_dir, folder);
                }
                debug!(group_jid, task_id, "container active, task queued");
                return;
            }

            if inner.active_count >= inner.max_concurrent {
                let state = inner.get_or_insert(group_jid);
                state.pending_tasks.push_back(QueuedTask {
                    id: task_id.to_string(),
                    group_jid: group_jid.to_string(),
                    task_fn,
                });
                let jid = group_jid.to_string();
                if !inner.waiting_groups.contains(&jid) {
                    inner.waiting_groups.push_back(jid);
                }
                debug!(
                    group_jid,
                    task_id,
                    active_count = inner.active_count,
                    "at concurrency limit, task queued"
                );
                return;
            }

            // Run immediately
            let state = inner.get_or_insert(group_jid);
            state.active = true;
            state.idle_waiting = false;
            state.is_task_container = true;
            inner.active_count += 1;

            Some(QueuedTask {
                id: task_id.to_string(),
                group_jid: group_jid.to_string(),
                task_fn,
            })
        };

        if let Some(task) = task_to_run {
            let queue = self.inner.clone();
            let jid = group_jid.to_string();
            tokio::spawn(async move {
                run_task(queue, jid, task).await;
            });
        }
    }

    /// Register a container process for a group.
    pub async fn register_process(
        &self,
        group_jid: &str,
        container_name: &str,
        group_folder: Option<&str>,
    ) {
        let mut inner = self.inner.lock().await;
        let state = inner.get_or_insert(group_jid);
        state.container_name = Some(container_name.to_string());
        if let Some(folder) = group_folder {
            state.group_folder = Some(folder.to_string());
        }
    }

    /// Mark the container as idle-waiting. Preempts if tasks are pending.
    pub async fn notify_idle(&self, group_jid: &str) {
        let mut inner = self.inner.lock().await;
        let has_tasks;
        let folder;
        {
            let state = inner.get_or_insert(group_jid);
            state.idle_waiting = true;
            has_tasks = !state.pending_tasks.is_empty();
            folder = state.group_folder.clone();
            // Mark pool container as idle (safe for health checks now)
            if let Some(ref mut pc) = state.pool_container {
                pc.active_delivery = false;
            }
        }
        if has_tasks {
            if let Some(ref f) = folder {
                write_close_sentinel(&inner.data_dir, f);
            }
        }
    }

    /// Send a follow-up message to the active container via IPC input file.
    pub async fn send_message(&self, group_jid: &str, text: &str) -> bool {
        let input_dir = {
            let inner = self.inner.lock().await;
            let state = match inner.groups.get(group_jid) {
                Some(s) => s,
                None => return false,
            };
            if !state.active || state.group_folder.is_none() || state.is_task_container {
                return false;
            }
            let folder = state.group_folder.as_ref().unwrap();
            if folder.is_empty() || folder.contains("..") || folder.contains('/') || folder.contains('\\') {
                error!(group_jid, folder = folder.as_str(), "refusing IPC message for invalid group_folder");
                return false;
            }
            inner.data_dir.join("ipc").join(folder).join("input")
        };

        write_ipc_message(&input_dir, text)
    }

    /// Signal the active container to wind down via close sentinel.
    pub async fn close_stdin(&self, group_jid: &str) {
        let inner = self.inner.lock().await;
        if let Some(state) = inner.groups.get(group_jid) {
            if state.active {
                if let Some(ref folder) = state.group_folder {
                    write_close_sentinel(&inner.data_dir, folder);
                }
            }
        }
    }

    /// Check if a group has an active container.
    pub async fn is_active(&self, group_jid: &str) -> bool {
        let inner = self.inner.lock().await;
        inner
            .groups
            .get(group_jid)
            .map(|s| s.active)
            .unwrap_or(false)
    }

    /// Stop an active container via `docker stop`.
    pub async fn kill_group(&self, group_jid: &str) -> bool {
        let container_name = {
            let inner = self.inner.lock().await;
            match inner.groups.get(group_jid) {
                Some(s) if s.active && s.container_name.is_some() => {
                    s.container_name.clone().unwrap()
                }
                _ => return false,
            }
        };

        match tokio::process::Command::new("docker")
            .args(["stop", &container_name])
            .output()
            .await
        {
            Ok(output) if output.status.success() => {
                info!(
                    group_jid,
                    container = container_name.as_str(),
                    "container stopped via kill_group"
                );
                true
            }
            Ok(_) => {
                warn!(
                    group_jid,
                    container = container_name.as_str(),
                    "failed to stop container"
                );
                false
            }
            Err(e) => {
                error!(group_jid, container = container_name.as_str(), err = %e, "docker stop error");
                false
            }
        }
    }

    /// Graceful shutdown — mark as shutting down, detach containers.
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        inner.shutting_down = true;

        let active_containers: Vec<String> = inner
            .groups
            .values()
            .filter_map(|s| {
                if s.active {
                    s.container_name.clone()
                } else {
                    None
                }
            })
            .collect();

        info!(
            active_count = inner.active_count,
            detached_containers = ?active_containers,
            "GroupQueue shutting down (containers detached, not killed)"
        );
    }

    /// Get the current active container count.
    pub async fn active_count(&self) -> usize {
        self.inner.lock().await.active_count
    }

    /// Check if a group has a warm (pool) container ready.
    pub async fn has_warm_container(&self, group_jid: &str) -> bool {
        let inner = self.inner.lock().await;
        inner
            .groups
            .get(group_jid)
            .and_then(|s| s.pool_container.as_ref())
            .is_some()
    }

    /// Store a warm container for a group. Called after pool_spawn() succeeds.
    pub async fn set_pool_container(&self, group_jid: &str, pc: PoolContainer) {
        let mut inner = self.inner.lock().await;
        let state = inner.get_or_insert(group_jid);
        state.pool_container = Some(pc);
        info!(group_jid, container = state.pool_container.as_ref().unwrap().container_name.as_str(), "warm container registered");
    }

    /// Send a message to a warm container via IPC file. Returns a receiver
    /// for output frames, or None if no warm container exists.
    pub async fn send_to_warm_container(
        &self,
        group_jid: &str,
        text: &str,
    ) -> Option<tokio::sync::mpsc::Receiver<ContainerOutput>> {
        let mut inner = self.inner.lock().await;

        // Extract what we need before the mutable borrow
        let data_dir = inner.data_dir.clone();
        let state = inner.groups.get_mut(group_jid)?;

        if state.pool_container.is_none() {
            return None;
        }

        // Validate and build input path
        let folder = state.group_folder.as_ref()?;
        if folder.is_empty() || folder.contains("..") || folder.contains('/') || folder.contains('\\') {
            error!(group_jid, folder = folder.as_str(), "refusing IPC message for invalid group_folder");
            return None;
        }
        let input_dir = data_dir.join("ipc").join(folder).join("input");
        if !write_ipc_message(&input_dir, text) {
            return None;
        }

        // Update last activity, mark as actively processing, and create delivery channel
        let pc = state.pool_container.as_mut().unwrap();
        pc.last_activity = Instant::now();
        pc.active_delivery = true;

        let (tx, rx) = tokio::sync::mpsc::channel(1000);
        pc.delivery_tx = tx;
        Some(rx)
    }

    /// Remove the warm container for a group (on crash, reaping, or shutdown).
    pub async fn clear_pool_container(&self, group_jid: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(state) = inner.groups.get_mut(group_jid) {
            if let Some(pc) = state.pool_container.take() {
                pc.exit_monitor.abort();
                pc.uds_listener.abort();
                info!(group_jid, container = pc.container_name.as_str(), "warm container cleared");
            }
        }
    }

    /// Get the data_dir path.
    pub async fn data_dir(&self) -> PathBuf {
        self.inner.lock().await.data_dir.clone()
    }

    /// Iterate all groups with warm containers for idle reaping.
    /// Returns (group_jid, group_folder, container_name, last_activity, active_delivery) for each warm container.
    pub async fn warm_containers(&self) -> Vec<(String, String, String, Instant, bool)> {
        let inner = self.inner.lock().await;
        inner.groups.iter()
            .filter_map(|(jid, state)| {
                let pc = state.pool_container.as_ref()?;
                let folder = state.group_folder.as_ref()?;
                Some((jid.clone(), folder.clone(), pc.container_name.clone(), pc.last_activity, pc.active_delivery))
            })
            .collect()
    }

    /// Purge stale IPC input files for a group (after container exit).
    pub async fn purge_stale_ipc(&self, group_folder: &str) {
        let data_dir = self.inner.lock().await.data_dir.clone();
        let input_dir = data_dir.join("ipc").join(group_folder).join("input");
        if let Ok(entries) = std::fs::read_dir(&input_dir) {
            let mut count = 0;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    if std::fs::remove_file(&path).is_ok() {
                        count += 1;
                    }
                }
            }
            if count > 0 {
                warn!(group_folder, count, "purged stale IPC input files");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal execution functions
// ---------------------------------------------------------------------------

async fn run_for_group(queue: Arc<Mutex<Inner>>, group_jid: String) {
    debug!(
        group_jid = group_jid.as_str(),
        "starting message processing for group"
    );

    let process_fn = {
        let inner = queue.lock().await;
        inner.process_messages_fn.clone()
    };

    // Catch panics so reset_group always runs (matches Node's try/finally).
    // Without this, a panic leaves active=true permanently, blocking the group.
    let success = if let Some(ref f) = process_fn {
        match std::panic::AssertUnwindSafe(f(group_jid.clone()))
            .catch_unwind()
            .await
        {
            Ok(result) => result,
            Err(_) => {
                error!(
                    group_jid = group_jid.as_str(),
                    "process_messages_fn panicked"
                );
                false
            }
        }
    } else {
        warn!(
            group_jid = group_jid.as_str(),
            "no process_messages_fn set, skipping"
        );
        false
    };

    let mut inner = queue.lock().await;

    if success {
        if let Some(state) = inner.groups.get_mut(&group_jid) {
            state.retry_count = 0;
        }
    } else {
        let retry_count = inner
            .groups
            .get(&group_jid)
            .map(|s| s.retry_count + 1)
            .unwrap_or(1);

        if let Some(state) = inner.groups.get_mut(&group_jid) {
            state.retry_count = retry_count;
        }

        if retry_count <= MAX_RETRIES {
            let delay_ms = BASE_RETRY_MS * 2u64.pow(retry_count - 1);
            info!(
                group_jid = group_jid.as_str(),
                retry_count,
                delay_ms,
                "scheduling retry with backoff"
            );
            let queue_clone = queue.clone();
            let jid_clone = group_jid.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                let mut inner = queue_clone.lock().await;
                if inner.shutting_down {
                    return;
                }
                let state = inner.get_or_insert(&jid_clone);
                state.pending_messages = true;
                // Trigger drain so the flag is actually picked up.
                // Without this, the group stays stuck until a new external message arrives.
                drain_pending(&mut inner, queue_clone.clone());
            });
        } else {
            error!(
                group_jid = group_jid.as_str(),
                retry_count,
                "max retries exceeded, dropping (will retry on next incoming message)"
            );
            if let Some(state) = inner.groups.get_mut(&group_jid) {
                state.retry_count = 0;
            }
        }
    }

    inner.reset_group(&group_jid);
    drain_pending(&mut inner, queue.clone());
}

/// After a group finishes, check for pending work on that group and waiting groups.
/// Tasks drain before messages (matching Node's drainGroup priority ordering).
fn drain_pending(inner: &mut Inner, queue: Arc<Mutex<Inner>>) {
    if inner.shutting_down {
        return;
    }

    let mut message_spawns: Vec<String> = Vec::new();
    let mut task_spawns: Vec<(String, QueuedTask)> = Vec::new();

    let candidates: Vec<String> = inner
        .groups
        .iter()
        .filter(|(_, s)| !s.active && (s.pending_messages || !s.pending_tasks.is_empty()))
        .map(|(jid, _)| jid.clone())
        .collect();

    for jid in candidates {
        if inner.active_count >= inner.max_concurrent {
            break;
        }
        let state = inner.get_or_insert(&jid);
        if state.active {
            continue;
        }

        // Tasks first (they won't be re-discovered from the database like messages)
        if let Some(task) = state.pending_tasks.pop_front() {
            state.active = true;
            state.idle_waiting = false;
            state.is_task_container = true;
            inner.active_count += 1;
            inner.waiting_groups.retain(|w| w != &jid);
            task_spawns.push((jid, task));
        } else if state.pending_messages {
            state.active = true;
            state.idle_waiting = false;
            state.is_task_container = false;
            state.pending_messages = false;
            inner.active_count += 1;
            inner.waiting_groups.retain(|w| w != &jid);
            message_spawns.push(jid);
        }
    }

    for jid in message_spawns {
        let queue_clone = queue.clone();
        tokio::spawn(async move {
            run_for_group(queue_clone, jid).await;
        });
    }
    for (jid, task) in task_spawns {
        let queue_clone = queue.clone();
        tokio::spawn(async move {
            run_task(queue_clone, jid, task).await;
        });
    }
}

async fn run_task(queue: Arc<Mutex<Inner>>, group_jid: String, task: QueuedTask) {
    debug!(
        group_jid = group_jid.as_str(),
        task_id = task.id.as_str(),
        "running queued task"
    );

    // Catch panics so reset_group always runs (matches Node's try/finally)
    if let Err(_) = std::panic::AssertUnwindSafe((task.task_fn)())
        .catch_unwind()
        .await
    {
        error!(
            group_jid = group_jid.as_str(),
            task_id = task.id.as_str(),
            "task_fn panicked"
        );
    }

    let mut inner = queue.lock().await;
    inner.reset_group(&group_jid);
    drain_pending(&mut inner, queue.clone());
}

// ---------------------------------------------------------------------------
// IPC helpers
// ---------------------------------------------------------------------------

fn write_ipc_message(input_dir: &Path, text: &str) -> bool {
    if let Err(e) = std::fs::create_dir_all(input_dir) {
        error!(err = %e, "failed to create IPC input dir");
        return false;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let filename = format!("{ts}-{:08x}.json", next_ipc_counter());
    let filepath = input_dir.join(&filename);
    let temp_path = input_dir.join(format!("{filename}.tmp"));

    let content = serde_json::json!({"type": "message", "text": text});
    match std::fs::write(&temp_path, content.to_string()) {
        Ok(()) => match std::fs::rename(&temp_path, &filepath) {
            Ok(()) => true,
            Err(e) => {
                error!(err = %e, "failed to rename IPC message file");
                false
            }
        },
        Err(e) => {
            error!(err = %e, "failed to write IPC message file");
            false
        }
    }
}

fn write_close_sentinel(data_dir: &Path, group_folder: &str) {
    if group_folder.is_empty() || group_folder.contains("..") || group_folder.contains('/') || group_folder.contains('\\') {
        error!(group_folder, "refusing close sentinel for invalid group_folder");
        return;
    }
    let input_dir = data_dir.join("ipc").join(group_folder).join("input");
    let _ = std::fs::create_dir_all(&input_dir);
    let _ = std::fs::write(input_dir.join("_close"), "");
}

/// Monotonic counter for IPC filename uniqueness (process-scoped).
static IPC_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_ipc_counter() -> u64 {
    IPC_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Pool idle reaping loop
// ---------------------------------------------------------------------------

/// Background task that reaps warm containers idle beyond the configured timeout.
/// Runs every 60s, checks last_activity for each warm container.
pub async fn run_pool_reaper(
    queue: Arc<GroupQueue>,
    idle_timeout_secs: u64,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let check_interval = std::time::Duration::from_secs(60);
    let idle_timeout = std::time::Duration::from_secs(idle_timeout_secs);

    let health_check_interval = 5u32; // Run health check every 5 ticks (5 min)
    let mut tick_count = 1u32; // Start at 1 so first tick doesn't trigger health check

    info!(idle_timeout_secs, "pool reaper started");

    loop {
        tokio::select! {
            _ = tokio::time::sleep(check_interval) => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("pool reaper shutting down");
                    // On shutdown: reap all warm containers
                    let containers = queue.warm_containers().await;
                    for (jid, folder, container_name, _, _) in &containers {
                        info!(container = container_name.as_str(), group = folder.as_str(), "shutdown: stopping warm container");
                        // Write close sentinel so container exits gracefully
                        let data_dir = queue.data_dir().await;
                        write_close_sentinel(&data_dir, folder);
                    }
                    // Give containers a grace period to exit
                    if !containers.is_empty() {
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        // Force-stop any that didn't exit
                        for (jid, folder, container_name, _, _) in &containers {
                            let _ = tokio::process::Command::new("docker")
                                .args(["stop", "-t", "5", container_name])
                                .output()
                                .await;
                            queue.purge_stale_ipc(folder).await;
                            queue.clear_pool_container(jid).await;
                        }
                    }
                    break;
                }
            }
        }

        tick_count += 1;
        let containers = queue.warm_containers().await;

        // Idle reaping
        for (jid, folder, container_name, last_activity, _) in &containers {
            if last_activity.elapsed() > idle_timeout {
                info!(
                    container = container_name.as_str(),
                    group = folder.as_str(),
                    idle_secs = last_activity.elapsed().as_secs(),
                    "reaping idle warm container"
                );
                let data_dir = queue.data_dir().await;
                write_close_sentinel(&data_dir, folder);
                // The exit monitor will handle cleanup when the container exits
            }
        }

        // Health check via ping/pong (every health_check_interval ticks)
        // Only ping containers that have been idle for at least 2 minutes — active containers
        // can't respond to pings because drainIpcInput() only runs between queries.
        if tick_count % health_check_interval == 0 && !containers.is_empty() {
            let data_dir = queue.data_dir().await;
            for (jid, folder, container_name, _, active_delivery) in &containers {
                // Skip containers actively processing a query — they can't respond to
                // pings because drainIpcInput() only runs between queries.
                if *active_delivery {
                    info!(
                        container = container_name.as_str(),
                        "skipping health check — container is actively processing"
                    );
                    continue;
                }
                let pong_path = data_dir.join("ipc").join(folder).join("pong.json");
                // Clean up any stale pong from a previous check BEFORE sending ping
                let _ = std::fs::remove_file(&pong_path);

                // Write ping to IPC input
                let input_dir = data_dir.join("ipc").join(folder).join("input");
                let ping_content = serde_json::json!({"type": "ping"});
                let ping_file = input_dir.join(format!("ping-{}.json", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()));
                let _ = std::fs::create_dir_all(&input_dir);
                let _ = std::fs::write(&ping_file, ping_content.to_string());

                let mut got_pong = false;
                for _ in 0..10 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if pong_path.exists() {
                        let _ = std::fs::remove_file(&pong_path);
                        got_pong = true;
                        break;
                    }
                }

                if !got_pong {
                    // Re-check live state: a message may have arrived during the 5s wait,
                    // making the container busy (can't respond to pings while running a query).
                    let still_idle = {
                        let live_containers = queue.warm_containers().await;
                        live_containers.iter()
                            .find(|(j, _, _, _, _)| j == jid)
                            .map(|(_, _, _, _, active)| !active)
                            .unwrap_or(false) // container gone = don't kill
                    };
                    if still_idle {
                        warn!(
                            container = container_name.as_str(),
                            group = folder.as_str(),
                            "health check failed (no pong within 5s) — stopping container"
                        );
                        let _ = tokio::process::Command::new("docker")
                            .args(["stop", "-t", "5", container_name])
                            .output()
                            .await;
                        queue.purge_stale_ipc(folder).await;
                        queue.clear_pool_container(jid).await;
                    } else {
                        info!(
                            container = container_name.as_str(),
                            "health check: no pong, but container became active — skipping kill"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_queue_has_zero_active() {
        let q = GroupQueue::new(3, PathBuf::from("/tmp/test-queue"));
        assert_eq!(q.active_count().await, 0);
    }

    #[tokio::test]
    async fn is_active_returns_false_for_unknown_group() {
        let q = GroupQueue::new(3, PathBuf::from("/tmp/test-queue"));
        assert!(!q.is_active("tg:unknown").await);
    }

    #[tokio::test]
    async fn shutdown_sets_flag() {
        let q = GroupQueue::new(3, PathBuf::from("/tmp/test-queue"));
        q.shutdown().await;
        // After shutdown, enqueue should be a no-op
        q.enqueue_message_check("tg:12345").await;
        assert!(!q.is_active("tg:12345").await);
    }

    #[test]
    fn counter_increments() {
        let a = next_ipc_counter();
        let b = next_ipc_counter();
        assert!(b > a, "counter should be monotonically increasing");
    }

    #[test]
    fn write_close_sentinel_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        write_close_sentinel(dir.path(), "test-group");
        let sentinel = dir
            .path()
            .join("ipc")
            .join("test-group")
            .join("input")
            .join("_close");
        assert!(sentinel.exists());
    }

    #[test]
    fn write_ipc_message_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let input_dir = dir.path().join("input");
        let result = write_ipc_message(&input_dir, "hello");
        assert!(result);
        let files: Vec<_> = std::fs::read_dir(&input_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "json")
            })
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[tokio::test]
    async fn has_warm_container_returns_false_initially() {
        let q = GroupQueue::new(3, PathBuf::from("/tmp/test-pool"));
        assert!(!q.has_warm_container("tg:123").await);
    }

    #[tokio::test]
    async fn set_and_clear_pool_container() {
        let q = GroupQueue::new(3, PathBuf::from("/tmp/test-pool"));
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        let pc = PoolContainer {
            container_name: "test-container".into(),
            child: tokio::process::Command::new("true").spawn().unwrap(),
            started_at: Instant::now(),
            last_activity: Instant::now(),
            active_delivery: false,
            exit_monitor: tokio::spawn(async {}),
            uds_listener: tokio::spawn(async {}),
            delivery_tx: tx,
        };
        q.set_pool_container("tg:123", pc).await;
        assert!(q.has_warm_container("tg:123").await);

        q.clear_pool_container("tg:123").await;
        assert!(!q.has_warm_container("tg:123").await);
    }

    #[tokio::test]
    async fn warm_containers_lists_pool_entries() {
        let q = GroupQueue::new(3, PathBuf::from("/tmp/test-pool"));

        // Register a group folder first
        q.register_process("tg:456", "cont-456", Some("group-a")).await;

        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        let pc = PoolContainer {
            container_name: "cont-456".into(),
            child: tokio::process::Command::new("true").spawn().unwrap(),
            started_at: Instant::now(),
            last_activity: Instant::now(),
            active_delivery: false,
            exit_monitor: tokio::spawn(async {}),
            uds_listener: tokio::spawn(async {}),
            delivery_tx: tx,
        };
        q.set_pool_container("tg:456", pc).await;

        let warm = q.warm_containers().await;
        assert_eq!(warm.len(), 1);
        assert_eq!(warm[0].0, "tg:456");
        assert_eq!(warm[0].1, "group-a");
        assert_eq!(warm[0].2, "cont-456");
    }

    #[tokio::test]
    async fn send_to_warm_container_returns_none_without_pool() {
        let q = GroupQueue::new(3, PathBuf::from("/tmp/test-pool"));
        let result = q.send_to_warm_container("tg:789", "hello").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn send_to_warm_container_writes_ipc_and_returns_rx() {
        let dir = tempfile::tempdir().unwrap();
        let q = GroupQueue::new(3, dir.path().to_path_buf());

        // Set up group state with folder
        q.register_process("tg:100", "cont-100", Some("group-b")).await;

        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        let pc = PoolContainer {
            container_name: "cont-100".into(),
            child: tokio::process::Command::new("true").spawn().unwrap(),
            started_at: Instant::now(),
            last_activity: Instant::now(),
            active_delivery: false,
            exit_monitor: tokio::spawn(async {}),
            uds_listener: tokio::spawn(async {}),
            delivery_tx: tx,
        };
        q.set_pool_container("tg:100", pc).await;

        let result = q.send_to_warm_container("tg:100", "test message").await;
        assert!(result.is_some());

        // Verify IPC file was written
        let input_dir = dir.path().join("ipc").join("group-b").join("input");
        let files: Vec<_> = std::fs::read_dir(&input_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[tokio::test]
    async fn purge_stale_ipc_removes_json_files() {
        let dir = tempfile::tempdir().unwrap();
        let q = GroupQueue::new(3, dir.path().to_path_buf());

        let input_dir = dir.path().join("ipc").join("test-purge").join("input");
        std::fs::create_dir_all(&input_dir).unwrap();
        std::fs::write(input_dir.join("stale.json"), "{}").unwrap();
        std::fs::write(input_dir.join("stale2.json"), "{}").unwrap();
        std::fs::write(input_dir.join("_close"), "").unwrap(); // Not JSON, should survive

        q.purge_stale_ipc("test-purge").await;

        let remaining: Vec<_> = std::fs::read_dir(&input_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].file_name().to_str().unwrap(), "_close");
    }
}
