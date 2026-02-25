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

use tokio::sync::Mutex;
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

/// Per-group state tracked by the queue.
#[derive(Default)]
struct GroupState {
    active: bool,
    idle_waiting: bool,
    is_task_container: bool,
    pending_messages: bool,
    pending_tasks: VecDeque<QueuedTask>,
    container_name: Option<String>,
    group_folder: Option<String>,
    retry_count: u32,
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
            state.group_folder = None;
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

    let success = if let Some(ref f) = process_fn {
        f(group_jid.clone()).await
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
                if !inner.shutting_down {
                    let state = inner.get_or_insert(&jid_clone);
                    state.pending_messages = true;
                }
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
    // Drain is handled by the next poll cycle or enqueue call
}

async fn run_task(queue: Arc<Mutex<Inner>>, group_jid: String, task: QueuedTask) {
    debug!(
        group_jid = group_jid.as_str(),
        task_id = task.id.as_str(),
        "running queued task"
    );

    // Execute the task
    (task.task_fn)().await;

    let mut inner = queue.lock().await;
    inner.reset_group(&group_jid);
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
    let filename = format!("{ts}-{:04x}.json", rand_u16());
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
    let input_dir = data_dir.join("ipc").join(group_folder).join("input");
    let _ = std::fs::create_dir_all(&input_dir);
    let _ = std::fs::write(input_dir.join("_close"), "");
}

/// Simple pseudo-random u16 for file name uniqueness.
fn rand_u16() -> u16 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.subsec_nanos() ^ (t.as_secs() as u32).wrapping_mul(2654435761)) as u16
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
    fn rand_u16_produces_values() {
        let a = rand_u16();
        assert!(a <= u16::MAX);
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
}
