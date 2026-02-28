//! Container protocol types shared between intercomd and HTTP endpoints.
//!
//! Defines the wire format for communication with agent containers:
//! - `ContainerInput`: JSON written to container stdin
//! - `ContainerOutput`: JSON extracted from stdout between OUTPUT markers
//! - `StreamEvent`: Incremental streaming events (tool starts, text deltas)

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::runtime::RuntimeKind;

/// Sentinel markers for robust output parsing.
/// Must match the constants in container agent-runner code.
pub const OUTPUT_START_MARKER: &str = "---INTERCOM_OUTPUT_START---";
pub const OUTPUT_END_MARKER: &str = "---INTERCOM_OUTPUT_END---";

/// Input payload written to container stdin as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerInput {
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub group_folder: String,
    pub chat_jid: String,
    pub is_main: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_scheduled_task: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Secrets injected via stdin, never written to disk.
    /// Zeroed from memory after writing to the container process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<HashMap<String, String>>,
}

/// Output payload extracted from container stdout between OUTPUT markers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerOutput {
    pub status: ContainerStatus,
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<StreamEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerStatus {
    Success,
    Error,
}

/// Incremental streaming event from the container.
/// Tag values use snake_case ("tool_start", "text_delta") to match Node wire format.
/// Field names use camelCase ("toolName", "toolInput") to match Node wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    ToolStart {
        #[serde(default, rename = "toolName")]
        tool_name: Option<String>,
        #[serde(default, rename = "toolInput")]
        tool_input: Option<String>,
    },
    TextDelta {
        #[serde(default)]
        text: Option<String>,
    },
}

/// Volume mount specification for container execution.
#[derive(Debug, Clone)]
pub struct VolumeMount {
    pub host_path: String,
    pub container_path: String,
    pub readonly: bool,
    /// Subdirectory names to hide via tmpfs overlay.
    pub exclude: Vec<String>,
}

/// Container image names keyed by runtime.
pub fn container_image(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Claude => "intercom-agent:latest",
        RuntimeKind::Gemini => "intercom-agent-gemini:latest",
        RuntimeKind::Codex => "intercom-agent-codex:latest",
    }
}

/// Runner source directory name for each runtime.
pub fn runner_dir_name(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Claude => "agent-runner",
        RuntimeKind::Gemini => "gemini-runner",
        RuntimeKind::Codex => "codex-runner",
    }
}

/// Container mount path for runner source code.
/// Claude uses flat layout at `/app/src`, others use nested layout.
pub fn runner_container_path(runtime: RuntimeKind) -> String {
    match runtime {
        RuntimeKind::Claude => "/app/src".to_string(),
        _ => format!("/app/{}/src", runner_dir_name(runtime)),
    }
}

/// Parses OUTPUT marker pairs from a byte buffer.
///
/// Returns a vec of extracted JSON strings and the number of bytes consumed.
/// Unconsumed bytes (incomplete marker pair) remain in the caller's buffer.
pub fn extract_output_markers(buf: &str) -> (Vec<String>, usize) {
    let mut results = Vec::new();
    let mut consumed = 0;

    let mut search_from = 0;
    loop {
        let start = match buf[search_from..].find(OUTPUT_START_MARKER) {
            Some(pos) => search_from + pos,
            None => break,
        };

        let after_start = start + OUTPUT_START_MARKER.len();
        let end = match buf[after_start..].find(OUTPUT_END_MARKER) {
            Some(pos) => after_start + pos,
            None => break, // incomplete pair, stop here
        };

        let json_str = buf[after_start..end].trim().to_string();
        results.push(json_str);

        consumed = end + OUTPUT_END_MARKER.len();
        search_from = consumed;
    }

    (results, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_input_serializes_camel_case() {
        let input = ContainerInput {
            prompt: "hello".to_string(),
            session_id: Some("sess-123".to_string()),
            group_folder: "main".to_string(),
            chat_jid: "tg:123".to_string(),
            is_main: true,
            is_scheduled_task: None,
            assistant_name: Some("Amtiskaw".to_string()),
            model: None,
            secrets: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("\"chatJid\""));
        assert!(json.contains("\"groupFolder\""));
        assert!(json.contains("\"isMain\""));
        assert!(json.contains("\"sessionId\""));
        // Optional None fields should be absent
        assert!(!json.contains("\"model\""));
        assert!(!json.contains("\"secrets\""));
    }

    #[test]
    fn container_output_deserializes_from_node_format() {
        let json = r#"{"status":"success","result":"Hello!","newSessionId":"sess-456"}"#;
        let output: ContainerOutput = serde_json::from_str(json).unwrap();
        assert_eq!(output.status, ContainerStatus::Success);
        assert_eq!(output.result.as_deref(), Some("Hello!"));
        assert_eq!(output.new_session_id.as_deref(), Some("sess-456"));
    }

    #[test]
    fn container_output_error_status() {
        let json = r#"{"status":"error","result":null,"error":"Container exited with code 1"}"#;
        let output: ContainerOutput = serde_json::from_str(json).unwrap();
        assert_eq!(output.status, ContainerStatus::Error);
        assert!(output.result.is_none());
        assert!(output.error.is_some());
    }

    #[test]
    fn stream_event_tool_start() {
        let json = r#"{"type":"tool_start","toolName":"Read","toolInput":"/path/to/file"}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::ToolStart { tool_name, tool_input } => {
                assert_eq!(tool_name.as_deref(), Some("Read"));
                assert_eq!(tool_input.as_deref(), Some("/path/to/file"));
            }
            _ => panic!("expected ToolStart"),
        }
    }

    #[test]
    fn stream_event_text_delta() {
        let json = r#"{"type":"text_delta","text":"Hello "}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        match event {
            StreamEvent::TextDelta { text } => {
                assert_eq!(text.as_deref(), Some("Hello "));
            }
            _ => panic!("expected TextDelta"),
        }
    }

    #[test]
    fn extract_markers_single_pair() {
        let buf = format!(
            "some noise {}{{\"status\":\"success\",\"result\":\"hi\"}}{}trailing",
            OUTPUT_START_MARKER, OUTPUT_END_MARKER
        );
        let (results, consumed) = extract_output_markers(&buf);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0],
            r#"{"status":"success","result":"hi"}"#
        );
        assert!(consumed > 0);
        assert_eq!(&buf[consumed..], "trailing");
    }

    #[test]
    fn extract_markers_multiple_pairs() {
        let buf = format!(
            "{s}{{\"status\":\"success\",\"result\":null}}{e}{s}{{\"status\":\"success\",\"result\":\"done\"}}{e}",
            s = OUTPUT_START_MARKER,
            e = OUTPUT_END_MARKER,
        );
        let (results, consumed) = extract_output_markers(&buf);
        assert_eq!(results.len(), 2);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn extract_markers_incomplete_pair() {
        let buf = format!(
            "{s}{{\"status\":\"success\"}}",
            s = OUTPUT_START_MARKER,
        );
        let (results, consumed) = extract_output_markers(&buf);
        assert_eq!(results.len(), 0);
        assert_eq!(consumed, 0);
    }

    #[test]
    fn extract_markers_empty_buffer() {
        let (results, consumed) = extract_output_markers("");
        assert_eq!(results.len(), 0);
        assert_eq!(consumed, 0);
    }

    #[test]
    fn container_image_names() {
        assert_eq!(container_image(RuntimeKind::Claude), "intercom-agent:latest");
        assert_eq!(container_image(RuntimeKind::Gemini), "intercom-agent-gemini:latest");
        assert_eq!(container_image(RuntimeKind::Codex), "intercom-agent-codex:latest");
    }

    #[test]
    fn runner_container_paths() {
        assert_eq!(runner_container_path(RuntimeKind::Claude), "/app/src");
        assert_eq!(runner_container_path(RuntimeKind::Gemini), "/app/gemini-runner/src");
        assert_eq!(runner_container_path(RuntimeKind::Codex), "/app/codex-runner/src");
    }

    #[test]
    fn container_output_with_stream_event() {
        let json = r#"{"status":"success","result":null,"event":{"type":"tool_start","toolName":"Bash","toolInput":"ls"}}"#;
        let output: ContainerOutput = serde_json::from_str(json).unwrap();
        assert!(output.event.is_some());
        match output.event.unwrap() {
            StreamEvent::ToolStart { tool_name, .. } => {
                assert_eq!(tool_name.as_deref(), Some("Bash"));
            }
            _ => panic!("expected ToolStart"),
        }
    }

    #[test]
    fn volume_mount_builder() {
        let mount = VolumeMount {
            host_path: "/home/mk/projects".to_string(),
            container_path: "/workspace/project".to_string(),
            readonly: true,
            exclude: vec!["node_modules".to_string()],
        };
        assert!(mount.readonly);
        assert_eq!(mount.exclude.len(), 1);
    }
}
