//! IPC types shared between container agents and the intercomd host.
//!
//! Containers write JSON files into `/workspace/ipc/{channel}/` directories.
//! The host daemon polls these directories, processes files, and (for queries)
//! writes response files that containers poll for.
//!
//! Three IPC channels:
//! - **messages**: outbound messages from containers (container → host → channel)
//! - **tasks**: task management commands (schedule, pause, resume, cancel, register_group)
//! - **queries**: Demarch kernel queries with UUID request/response pattern

use serde::{Deserialize, Serialize};

/// Outbound message from a container agent to a messaging channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcMessage {
    /// Must be "message".
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Target chat JID (e.g., "tg:1108701034").
    #[serde(rename = "chatJid")]
    pub chat_jid: String,
    /// Message text content.
    pub text: String,
    /// Optional sender identity override.
    pub sender: Option<String>,
    /// Source group folder (set by container).
    #[serde(rename = "groupFolder")]
    pub group_folder: Option<String>,
    pub timestamp: Option<String>,
}

/// Task management command from a container agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcTask {
    ScheduleTask {
        prompt: String,
        schedule_type: String,
        schedule_value: String,
        #[serde(default = "default_context_mode")]
        context_mode: String,
        #[serde(rename = "targetJid")]
        target_jid: Option<String>,
        #[serde(rename = "createdBy")]
        created_by: Option<String>,
        timestamp: Option<String>,
    },
    PauseTask {
        #[serde(rename = "taskId")]
        task_id: String,
        #[serde(rename = "groupFolder")]
        group_folder: Option<String>,
        timestamp: Option<String>,
    },
    ResumeTask {
        #[serde(rename = "taskId")]
        task_id: String,
        #[serde(rename = "groupFolder")]
        group_folder: Option<String>,
        timestamp: Option<String>,
    },
    CancelTask {
        #[serde(rename = "taskId")]
        task_id: String,
        #[serde(rename = "groupFolder")]
        group_folder: Option<String>,
        timestamp: Option<String>,
    },
    RefreshGroups {
        timestamp: Option<String>,
    },
    RegisterGroup {
        jid: String,
        name: String,
        folder: String,
        trigger: String,
        timestamp: Option<String>,
    },
}

fn default_context_mode() -> String {
    "isolated".to_string()
}

/// Demarch kernel query from a container agent.
/// Container writes `{uuid}.json` to `queries/`, host writes response to `responses/{uuid}.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcQuery {
    /// Unique request ID — used as the response filename.
    pub uuid: String,
    /// Query type: run_status, sprint_phase, search_beads, spec_lookup,
    /// review_summary, next_work, run_events.
    #[serde(rename = "type")]
    pub query_type: String,
    /// Type-specific parameters.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Response to a Demarch kernel query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcQueryResponse {
    pub status: String,
    pub result: String,
}

impl IpcQueryResponse {
    pub fn ok(result: impl Into<String>) -> Self {
        Self {
            status: "ok".to_string(),
            result: result.into(),
        }
    }

    pub fn error(result: impl Into<String>) -> Self {
        Self {
            status: "error".to_string(),
            result: result.into(),
        }
    }
}

/// Context for authorization decisions — derived from the IPC directory path.
#[derive(Debug, Clone)]
pub struct IpcGroupContext {
    /// Group folder name (e.g., "main", "team-eng").
    pub group_folder: String,
    /// Whether this is the main group (has elevated privileges).
    pub is_main: bool,
}

impl IpcGroupContext {
    pub fn new(group_folder: impl Into<String>, main_group_name: &str) -> Self {
        let group_folder = group_folder.into();
        let is_main = group_folder == main_group_name;
        Self {
            group_folder,
            is_main,
        }
    }
}
