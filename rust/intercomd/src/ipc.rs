//! Filesystem-based IPC watcher for intercomd.
//!
//! Polls `{ipc_base}/{group}/` directories for messages, tasks, and queries.
//! Processes files atomically (read → act → unlink), moving failures to an
//! `errors/` directory for debugging.
//!
//! Authorization model:
//! - Main group can send messages to any chat and manage any task.
//! - Non-main groups can only send to their own registered chat JID.
//! - Demarch query authorization delegated to DemarchAdapter (allowlist + is_main).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use intercom_core::{
    DemarchAdapter, IpcGroupContext, IpcMessage, IpcQuery, IpcQueryResponse, IpcTask,
    ReadOperation, WriteOperation,
};
use tracing::{debug, error, info, warn};

const MAIN_GROUP_FOLDER: &str = "main";

/// Configuration for the IPC watcher.
#[derive(Debug, Clone)]
pub struct IpcWatcherConfig {
    /// Base directory for IPC files (e.g., `data/ipc`).
    pub ipc_base_dir: PathBuf,
    /// Poll interval.
    pub poll_interval: Duration,
}

impl Default for IpcWatcherConfig {
    fn default() -> Self {
        Self {
            ipc_base_dir: PathBuf::from("data/ipc"),
            poll_interval: Duration::from_secs(1),
        }
    }
}

/// Callback trait for IPC actions that need the Node host.
///
/// During the strangler-fig migration, some IPC actions (sending messages,
/// managing tasks) still need the Node host. This trait allows intercomd to
/// delegate those actions while handling Demarch queries natively.
pub trait IpcDelegate: Send + Sync {
    /// Send a message to a chat JID via the messaging channel.
    fn send_message(&self, chat_jid: &str, text: &str, sender: Option<&str>);

    /// Forward a task command to the Node host for processing.
    fn forward_task(&self, task: &IpcTask, group_folder: &str, is_main: bool);
}

/// No-op delegate that logs actions without forwarding to Node.
/// Used when intercomd runs standalone (no Node host).
pub struct LogOnlyDelegate;

impl IpcDelegate for LogOnlyDelegate {
    fn send_message(&self, chat_jid: &str, text: &str, _sender: Option<&str>) {
        info!(
            chat_jid,
            text_len = text.len(),
            "IPC message received (no delegate — logged only)"
        );
    }

    fn forward_task(&self, task: &IpcTask, group_folder: &str, is_main: bool) {
        info!(
            ?task,
            group_folder,
            is_main,
            "IPC task received (no delegate — logged only)"
        );
    }
}

/// The IPC watcher. Owns polling state and dispatches to DemarchAdapter + delegate.
pub struct IpcWatcher {
    config: IpcWatcherConfig,
    demarch: Arc<DemarchAdapter>,
    delegate: Arc<dyn IpcDelegate>,
}

impl IpcWatcher {
    pub fn new(
        config: IpcWatcherConfig,
        demarch: Arc<DemarchAdapter>,
        delegate: Arc<dyn IpcDelegate>,
    ) -> Self {
        Self {
            config,
            demarch,
            delegate,
        }
    }

    /// Run the IPC polling loop. Call from a tokio::spawn.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        fs::create_dir_all(&self.config.ipc_base_dir).ok();
        info!(dir = %self.config.ipc_base_dir.display(), "IPC watcher started");

        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.config.poll_interval) => {
                    self.poll_once();
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("IPC watcher shutting down");
                        return;
                    }
                }
            }
        }
    }

    /// Process one polling cycle across all group directories.
    fn poll_once(&self) {
        let group_folders = match fs::read_dir(&self.config.ipc_base_dir) {
            Ok(entries) => entries
                .flatten()
                .filter(|entry| {
                    entry.file_type().is_ok_and(|ft| ft.is_dir())
                        && entry.file_name() != "errors"
                })
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            Err(err) => {
                debug!(err = %err, "IPC base directory not readable");
                return;
            }
        };

        for group_folder in group_folders {
            let ctx = IpcGroupContext::new(&group_folder, MAIN_GROUP_FOLDER);
            let group_dir = self.config.ipc_base_dir.join(&group_folder);

            self.process_messages(&group_dir, &ctx);
            self.process_tasks(&group_dir, &ctx);
            self.process_queries(&group_dir, &ctx);
        }
    }

    /// Process outbound messages from `{group}/messages/`.
    fn process_messages(&self, group_dir: &Path, ctx: &IpcGroupContext) {
        let messages_dir = group_dir.join("messages");
        let files = match read_json_files(&messages_dir) {
            Some(files) => files,
            None => return,
        };

        for file_path in files {
            match read_and_parse::<IpcMessage>(&file_path) {
                Ok(msg) => {
                    if msg.msg_type != "message" || msg.chat_jid.is_empty() || msg.text.is_empty() {
                        warn!(path = %file_path.display(), "Invalid IPC message — missing fields");
                        move_to_errors(&self.config.ipc_base_dir, &file_path, &ctx.group_folder);
                        continue;
                    }

                    // Authorization: main can send anywhere, others only to their own chat
                    if ctx.is_main || self.is_authorized_target(&msg.chat_jid, &ctx.group_folder) {
                        self.delegate.send_message(
                            &msg.chat_jid,
                            &msg.text,
                            msg.sender.as_deref(),
                        );
                        debug!(
                            chat_jid = %msg.chat_jid,
                            group = %ctx.group_folder,
                            "IPC message dispatched"
                        );
                    } else {
                        warn!(
                            chat_jid = %msg.chat_jid,
                            group = %ctx.group_folder,
                            "Unauthorized IPC message attempt blocked"
                        );
                    }

                    remove_file(&file_path);
                }
                Err(err) => {
                    error!(path = %file_path.display(), err = %err, "Failed to parse IPC message");
                    move_to_errors(&self.config.ipc_base_dir, &file_path, &ctx.group_folder);
                }
            }
        }
    }

    /// Process task commands from `{group}/tasks/`.
    fn process_tasks(&self, group_dir: &Path, ctx: &IpcGroupContext) {
        let tasks_dir = group_dir.join("tasks");
        let files = match read_json_files(&tasks_dir) {
            Some(files) => files,
            None => return,
        };

        for file_path in files {
            match read_and_parse::<IpcTask>(&file_path) {
                Ok(task) => {
                    self.delegate
                        .forward_task(&task, &ctx.group_folder, ctx.is_main);
                    remove_file(&file_path);
                }
                Err(err) => {
                    error!(path = %file_path.display(), err = %err, "Failed to parse IPC task");
                    move_to_errors(&self.config.ipc_base_dir, &file_path, &ctx.group_folder);
                }
            }
        }
    }

    /// Process Demarch kernel queries from `{group}/queries/`.
    /// Writes responses to `{group}/responses/{uuid}.json`.
    fn process_queries(&self, group_dir: &Path, ctx: &IpcGroupContext) {
        let queries_dir = group_dir.join("queries");
        let responses_dir = group_dir.join("responses");
        let files = match read_json_files(&queries_dir) {
            Some(files) => files,
            None => return,
        };

        for file_path in files {
            match read_and_parse::<IpcQuery>(&file_path) {
                Ok(query) => {
                    if query.uuid.is_empty() || query.query_type.is_empty() {
                        warn!(
                            path = %file_path.display(),
                            group = %ctx.group_folder,
                            "Invalid query — missing uuid or type"
                        );
                        remove_file(&file_path);
                        continue;
                    }

                    let response = self.handle_query(&query, ctx);

                    // Write response atomically: write to .tmp then rename
                    if let Err(err) = write_response(&responses_dir, &query.uuid, &response) {
                        error!(
                            uuid = %query.uuid,
                            err = %err,
                            "Failed to write query response"
                        );
                    }

                    remove_file(&file_path);
                    debug!(
                        query_type = %query.query_type,
                        uuid = %query.uuid,
                        group = %ctx.group_folder,
                        status = %response.status,
                        "Demarch query processed"
                    );
                }
                Err(err) => {
                    error!(
                        path = %file_path.display(),
                        err = %err,
                        "Failed to parse Demarch query"
                    );
                    move_to_errors(&self.config.ipc_base_dir, &file_path, &ctx.group_folder);
                }
            }
        }
    }

    /// Route a query to the appropriate DemarchAdapter operation.
    fn handle_query(&self, query: &IpcQuery, ctx: &IpcGroupContext) -> IpcQueryResponse {
        let params = &query.params;

        match query.query_type.as_str() {
            "run_status" => {
                let run_id = params.get("runId").and_then(|v| v.as_str()).map(String::from);
                let resp = self.demarch.execute_read(ReadOperation::RunStatus { run_id });
                response_from_demarch(resp)
            }
            "sprint_phase" => {
                let resp = self.demarch.execute_read(ReadOperation::SprintPhase);
                response_from_demarch(resp)
            }
            "search_beads" => {
                let id = params.get("id").and_then(|v| v.as_str()).map(String::from);
                let query_str = params.get("query").and_then(|v| v.as_str()).map(String::from);
                let status = params.get("status").and_then(|v| v.as_str()).map(String::from);
                let resp = self.demarch.execute_read(ReadOperation::SearchBeads {
                    id,
                    query: query_str,
                    status,
                });
                response_from_demarch(resp)
            }
            "spec_lookup" => {
                let artifact_id = params
                    .get("artifactId")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let resp = self
                    .demarch
                    .execute_read(ReadOperation::SpecLookup { artifact_id });
                response_from_demarch(resp)
            }
            "review_summary" => {
                let resp = self.demarch.execute_read(ReadOperation::ReviewSummary);
                response_from_demarch(resp)
            }
            "next_work" => {
                let resp = self.demarch.execute_read(ReadOperation::NextWork);
                response_from_demarch(resp)
            }
            "run_events" => {
                let limit = params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let since = params
                    .get("since")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let resp = self
                    .demarch
                    .execute_read(ReadOperation::RunEvents { limit, since });
                response_from_demarch(resp)
            }

            // Write operations (require main group check)
            "create_issue" => {
                let title = params
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if title.is_empty() {
                    return IpcQueryResponse::error("create_issue requires a title");
                }
                let resp = self.demarch.execute_write(
                    WriteOperation::CreateIssue {
                        title,
                        description: params
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        priority: params
                            .get("priority")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        issue_type: params
                            .get("issue_type")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        labels: params.get("labels").and_then(|v| {
                            v.as_array().map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                        }),
                    },
                    ctx.is_main,
                );
                response_from_demarch(resp)
            }
            "update_issue" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    return IpcQueryResponse::error("update_issue requires an id");
                }
                let resp = self.demarch.execute_write(
                    WriteOperation::UpdateIssue {
                        id,
                        status: params
                            .get("status")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        priority: params
                            .get("priority")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        title: params
                            .get("title")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        description: params
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        notes: params
                            .get("notes")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    },
                    ctx.is_main,
                );
                response_from_demarch(resp)
            }
            "close_issue" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    return IpcQueryResponse::error("close_issue requires an id");
                }
                let resp = self.demarch.execute_write(
                    WriteOperation::CloseIssue {
                        id,
                        reason: params
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    },
                    ctx.is_main,
                );
                response_from_demarch(resp)
            }
            "start_run" => {
                let resp = self.demarch.execute_write(
                    WriteOperation::StartRun {
                        title: params
                            .get("title")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        description: params
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    },
                    ctx.is_main,
                );
                response_from_demarch(resp)
            }
            "approve_gate" => {
                let resp = self.demarch.execute_write(
                    WriteOperation::ApproveGate {
                        gate_id: params
                            .get("gate_id")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        reason: params
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                    },
                    ctx.is_main,
                );
                response_from_demarch(resp)
            }

            unknown => IpcQueryResponse::error(format!("Unknown query type: {unknown}")),
        }
    }

    /// Check if a non-main group is authorized to send to a given chat JID.
    /// Placeholder — in production this would check registered groups.
    fn is_authorized_target(&self, _chat_jid: &str, _group_folder: &str) -> bool {
        // TODO: Wire to registered groups state when available in Rust.
        // For now, reject non-main cross-group messages (safe default).
        false
    }
}

fn response_from_demarch(resp: intercom_core::DemarchResponse) -> IpcQueryResponse {
    match resp.status {
        intercom_core::DemarchStatus::Ok => IpcQueryResponse::ok(resp.result),
        intercom_core::DemarchStatus::Error => IpcQueryResponse::error(resp.result),
    }
}

// ── Filesystem helpers ─────────────────────────────────────────────

/// Read sorted `.json` filenames from a directory. Returns None if dir doesn't exist.
fn read_json_files(dir: &Path) -> Option<Vec<PathBuf>> {
    if !dir.exists() {
        return None;
    }

    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
                .collect();
            files.sort();
            Some(files)
        }
        Err(err) => {
            error!(dir = %dir.display(), err = %err, "Failed to read IPC directory");
            None
        }
    }
}

/// Read and parse a JSON file.
fn read_and_parse<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let content = fs::read_to_string(path)?;
    let parsed = serde_json::from_str(&content)?;
    Ok(parsed)
}

/// Write a query response atomically (write .tmp then rename).
fn write_response(
    responses_dir: &Path,
    uuid: &str,
    response: &IpcQueryResponse,
) -> anyhow::Result<()> {
    fs::create_dir_all(responses_dir)?;
    let response_path = responses_dir.join(format!("{uuid}.json"));
    let temp_path = responses_dir.join(format!("{uuid}.json.tmp"));
    let content = serde_json::to_string_pretty(response)?;
    fs::write(&temp_path, content)?;
    fs::rename(&temp_path, &response_path)?;
    Ok(())
}

/// Move a failed file to the errors directory for debugging.
fn move_to_errors(ipc_base: &Path, file_path: &Path, group_folder: &str) {
    let error_dir = ipc_base.join("errors");
    fs::create_dir_all(&error_dir).ok();

    if let Some(filename) = file_path.file_name() {
        let dest = error_dir.join(format!("{group_folder}-{}", filename.to_string_lossy()));
        if let Err(err) = fs::rename(file_path, &dest) {
            error!(
                path = %file_path.display(),
                err = %err,
                "Failed to move error file"
            );
        }
    }
}

/// Remove a processed file, ignoring errors.
fn remove_file(path: &Path) {
    if let Err(err) = fs::remove_file(path) {
        debug!(path = %path.display(), err = %err, "Failed to remove processed IPC file");
    }
}

// ── Collected group tracking (placeholder for registered-groups state) ──

/// Tracks which chat JIDs belong to which group folders.
/// Used for authorization of non-main message sends.
#[derive(Debug, Default)]
pub struct GroupRegistry {
    /// Map from chat_jid → group_folder.
    jid_to_folder: std::collections::HashMap<String, String>,
}

impl GroupRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, chat_jid: String, group_folder: String) {
        self.jid_to_folder.insert(chat_jid, group_folder);
    }

    pub fn folder_for_jid(&self, chat_jid: &str) -> Option<&str> {
        self.jid_to_folder.get(chat_jid).map(|s| s.as_str())
    }

    pub fn registered_jids(&self) -> HashSet<String> {
        self.jid_to_folder.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use intercom_core::{DemarchResponse, DemarchStatus, IpcQueryResponse};

    use super::*;

    #[test]
    fn ipc_group_context_detects_main() {
        let ctx = IpcGroupContext::new("main", "main");
        assert!(ctx.is_main);

        let ctx = IpcGroupContext::new("team-eng", "main");
        assert!(!ctx.is_main);
    }

    #[test]
    fn response_from_demarch_ok() {
        let demarch = DemarchResponse::ok("test result");
        let ipc = super::response_from_demarch(demarch);
        assert_eq!(ipc.status, "ok");
        assert_eq!(ipc.result, "test result");
    }

    #[test]
    fn response_from_demarch_error() {
        let demarch = DemarchResponse::error("test error");
        let ipc = super::response_from_demarch(demarch);
        assert_eq!(ipc.status, "error");
        assert_eq!(ipc.result, "test error");
    }

    #[test]
    fn atomic_response_write() {
        let tmp = tempfile::tempdir().unwrap();
        let responses_dir = tmp.path().join("responses");
        let response = IpcQueryResponse::ok("hello");

        write_response(&responses_dir, "abc-123", &response).unwrap();

        let written = fs::read_to_string(responses_dir.join("abc-123.json")).unwrap();
        let parsed: IpcQueryResponse = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed.status, "ok");
        assert_eq!(parsed.result, "hello");

        // .tmp file should not exist
        assert!(!responses_dir.join("abc-123.json.tmp").exists());
    }

    #[test]
    fn move_to_errors_preserves_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ipc_base = tmp.path();
        let file_path = ipc_base.join("test-query.json");
        fs::write(&file_path, "bad json").unwrap();

        move_to_errors(ipc_base, &file_path, "team-eng");

        assert!(!file_path.exists());
        assert!(ipc_base.join("errors/team-eng-test-query.json").exists());
    }

    #[test]
    fn group_registry_tracks_jids() {
        let mut registry = GroupRegistry::new();
        registry.register("tg:123".to_string(), "team-eng".to_string());
        registry.register("tg:456".to_string(), "main".to_string());

        assert_eq!(registry.folder_for_jid("tg:123"), Some("team-eng"));
        assert_eq!(registry.folder_for_jid("tg:456"), Some("main"));
        assert_eq!(registry.folder_for_jid("tg:999"), None);
        assert_eq!(registry.registered_jids().len(), 2);
    }

    #[test]
    fn read_json_files_returns_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        fs::write(dir.join("003-xyz.json"), "{}").unwrap();
        fs::write(dir.join("001-abc.json"), "{}").unwrap();
        fs::write(dir.join("002-def.json"), "{}").unwrap();
        fs::write(dir.join("readme.txt"), "not json").unwrap();

        let files = read_json_files(dir).unwrap();
        assert_eq!(files.len(), 3);
        assert!(files[0].ends_with("001-abc.json"));
        assert!(files[1].ends_with("002-def.json"));
        assert!(files[2].ends_with("003-xyz.json"));
    }

    #[test]
    fn read_json_files_nonexistent_dir_returns_none() {
        assert!(read_json_files(Path::new("/nonexistent/path")).is_none());
    }

    #[test]
    fn parse_ipc_query_from_json() {
        let json = r#"{"uuid": "abc-123", "type": "run_status", "params": {"runId": "5953m6kz"}}"#;
        let query: IpcQuery = serde_json::from_str(json).unwrap();
        assert_eq!(query.uuid, "abc-123");
        assert_eq!(query.query_type, "run_status");
        assert_eq!(
            query.params.get("runId").unwrap().as_str().unwrap(),
            "5953m6kz"
        );
    }

    #[test]
    fn parse_ipc_message_from_json() {
        let json = r#"{
            "type": "message",
            "chatJid": "tg:1108701034",
            "text": "Hello from agent",
            "sender": "Amtiskaw",
            "groupFolder": "main",
            "timestamp": "2026-02-25T12:00:00Z"
        }"#;
        let msg: IpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.chat_jid, "tg:1108701034");
        assert_eq!(msg.text, "Hello from agent");
        assert_eq!(msg.sender.as_deref(), Some("Amtiskaw"));
    }

    #[test]
    fn parse_ipc_task_schedule() {
        let json = r#"{
            "type": "schedule_task",
            "prompt": "Check build status",
            "schedule_type": "cron",
            "schedule_value": "0 9 * * *",
            "context_mode": "group",
            "targetJid": "tg:123",
            "timestamp": "2026-02-25T12:00:00Z"
        }"#;
        let task: IpcTask = serde_json::from_str(json).unwrap();
        match task {
            IpcTask::ScheduleTask {
                prompt,
                schedule_type,
                schedule_value,
                context_mode,
                ..
            } => {
                assert_eq!(prompt, "Check build status");
                assert_eq!(schedule_type, "cron");
                assert_eq!(schedule_value, "0 9 * * *");
                assert_eq!(context_mode, "group");
            }
            _ => panic!("Expected ScheduleTask"),
        }
    }

    #[test]
    fn parse_ipc_task_cancel() {
        let json = r#"{"type": "cancel_task", "taskId": "task-12345"}"#;
        let task: IpcTask = serde_json::from_str(json).unwrap();
        match task {
            IpcTask::CancelTask { task_id, .. } => {
                assert_eq!(task_id, "task-12345");
            }
            _ => panic!("Expected CancelTask"),
        }
    }

    #[test]
    fn poll_once_processes_query_and_writes_response() {
        use intercom_core::config::DemarchConfig;

        let tmp = tempfile::tempdir().unwrap();
        let ipc_base = tmp.path().to_path_buf();

        // Create a query file in main/queries/
        let queries_dir = ipc_base.join("main/queries");
        fs::create_dir_all(&queries_dir).unwrap();
        let query = serde_json::json!({
            "uuid": "test-uuid-001",
            "type": "next_work",
            "params": {}
        });
        fs::write(
            queries_dir.join("001-query.json"),
            serde_json::to_string(&query).unwrap(),
        )
        .unwrap();

        // Build watcher with a DemarchAdapter (CLIs won't be available, so
        // we'll get an error response — but the mechanics work end-to-end)
        let demarch_config = DemarchConfig::default();
        let demarch = Arc::new(DemarchAdapter::new(demarch_config, "."));
        let delegate: Arc<dyn IpcDelegate> = Arc::new(LogOnlyDelegate);
        let watcher = IpcWatcher::new(
            IpcWatcherConfig {
                ipc_base_dir: ipc_base.clone(),
                ..Default::default()
            },
            demarch,
            delegate,
        );

        // Run one poll cycle
        watcher.poll_once();

        // Query file should be consumed
        assert!(!queries_dir.join("001-query.json").exists());

        // Response file should exist
        let response_path = ipc_base.join("main/responses/test-uuid-001.json");
        assert!(response_path.exists());

        let response: IpcQueryResponse =
            serde_json::from_str(&fs::read_to_string(&response_path).unwrap()).unwrap();
        // bd won't be available in CI, so we expect an error response
        assert_eq!(response.status, "error");
    }

    #[test]
    fn poll_once_moves_bad_json_to_errors() {
        use intercom_core::config::DemarchConfig;

        let tmp = tempfile::tempdir().unwrap();
        let ipc_base = tmp.path().to_path_buf();

        // Create a malformed query file
        let queries_dir = ipc_base.join("main/queries");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("bad.json"), "not valid json {{{").unwrap();

        let demarch = Arc::new(DemarchAdapter::new(DemarchConfig::default(), "."));
        let delegate: Arc<dyn IpcDelegate> = Arc::new(LogOnlyDelegate);
        let watcher = IpcWatcher::new(
            IpcWatcherConfig {
                ipc_base_dir: ipc_base.clone(),
                ..Default::default()
            },
            demarch,
            delegate,
        );

        watcher.poll_once();

        // Bad file should be moved to errors/
        assert!(!queries_dir.join("bad.json").exists());
        assert!(ipc_base.join("errors/main-bad.json").exists());
    }

    #[test]
    fn poll_once_dispatches_message_for_main_group() {
        use intercom_core::config::DemarchConfig;
        use std::sync::Mutex;

        #[derive(Default)]
        struct RecordingDelegate {
            messages: Mutex<Vec<(String, String)>>,
        }

        impl IpcDelegate for RecordingDelegate {
            fn send_message(&self, chat_jid: &str, text: &str, _sender: Option<&str>) {
                self.messages
                    .lock()
                    .unwrap()
                    .push((chat_jid.to_string(), text.to_string()));
            }

            fn forward_task(&self, _task: &IpcTask, _group_folder: &str, _is_main: bool) {}
        }

        let tmp = tempfile::tempdir().unwrap();
        let ipc_base = tmp.path().to_path_buf();

        // Create a message file in main/messages/
        let messages_dir = ipc_base.join("main/messages");
        fs::create_dir_all(&messages_dir).unwrap();
        let msg = serde_json::json!({
            "type": "message",
            "chatJid": "tg:99999",
            "text": "Hello from test",
            "timestamp": "2026-02-25T12:00:00Z"
        });
        fs::write(
            messages_dir.join("001-msg.json"),
            serde_json::to_string(&msg).unwrap(),
        )
        .unwrap();

        let demarch = Arc::new(DemarchAdapter::new(DemarchConfig::default(), "."));
        let delegate = Arc::new(RecordingDelegate::default());
        let watcher = IpcWatcher::new(
            IpcWatcherConfig {
                ipc_base_dir: ipc_base.clone(),
                ..Default::default()
            },
            demarch,
            delegate.clone(),
        );

        watcher.poll_once();

        // Message should be consumed
        assert!(!messages_dir.join("001-msg.json").exists());

        // Delegate should have received the message
        let messages = delegate.messages.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "tg:99999");
        assert_eq!(messages[0].1, "Hello from test");
    }

    #[test]
    fn poll_once_blocks_unauthorized_message_from_non_main() {
        use intercom_core::config::DemarchConfig;
        use std::sync::Mutex;

        #[derive(Default)]
        struct RecordingDelegate {
            messages: Mutex<Vec<(String, String)>>,
        }

        impl IpcDelegate for RecordingDelegate {
            fn send_message(&self, chat_jid: &str, text: &str, _sender: Option<&str>) {
                self.messages
                    .lock()
                    .unwrap()
                    .push((chat_jid.to_string(), text.to_string()));
            }

            fn forward_task(&self, _task: &IpcTask, _group_folder: &str, _is_main: bool) {}
        }

        let tmp = tempfile::tempdir().unwrap();
        let ipc_base = tmp.path().to_path_buf();

        // Create a message in team-eng/messages/ (non-main group)
        let messages_dir = ipc_base.join("team-eng/messages");
        fs::create_dir_all(&messages_dir).unwrap();
        let msg = serde_json::json!({
            "type": "message",
            "chatJid": "tg:99999",
            "text": "Should be blocked",
            "timestamp": "2026-02-25T12:00:00Z"
        });
        fs::write(
            messages_dir.join("001-msg.json"),
            serde_json::to_string(&msg).unwrap(),
        )
        .unwrap();

        let demarch = Arc::new(DemarchAdapter::new(DemarchConfig::default(), "."));
        let delegate = Arc::new(RecordingDelegate::default());
        let watcher = IpcWatcher::new(
            IpcWatcherConfig {
                ipc_base_dir: ipc_base.clone(),
                ..Default::default()
            },
            demarch,
            delegate.clone(),
        );

        watcher.poll_once();

        // Message file should still be consumed (processed but rejected)
        assert!(!messages_dir.join("001-msg.json").exists());

        // But delegate should NOT have received it (blocked by auth)
        let messages = delegate.messages.lock().unwrap();
        assert_eq!(messages.len(), 0);
    }
}
