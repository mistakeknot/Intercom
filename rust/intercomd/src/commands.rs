//! Slash command handler for Telegram/WhatsApp commands.
//!
//! Port of the command handlers from `src/index.ts`.
//! Commands: /help, /status, /model, /reset (/new alias).

use std::time::Instant;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub runtime: String,
    pub display_name: String,
}

/// Static model catalog — mirrors config.ts MODEL_CATALOG.
pub fn model_catalog() -> Vec<ModelEntry> {
    vec![
        ModelEntry {
            id: "claude-opus-4-6".into(),
            runtime: "claude".into(),
            display_name: "Claude Opus 4.6".into(),
        },
        ModelEntry {
            id: "claude-sonnet-4-6".into(),
            runtime: "claude".into(),
            display_name: "Claude Sonnet 4.6".into(),
        },
        ModelEntry {
            id: "gemini-3.1-pro".into(),
            runtime: "gemini".into(),
            display_name: "Gemini 3.1 Pro".into(),
        },
        ModelEntry {
            id: "gemini-2.5-flash".into(),
            runtime: "gemini".into(),
            display_name: "Gemini 2.5 Flash".into(),
        },
        ModelEntry {
            id: "gpt-5.3-codex".into(),
            runtime: "codex".into(),
            display_name: "GPT-5.3 Codex".into(),
        },
    ]
}

pub const DEFAULT_MODEL: &str = "claude-opus-4-6";
pub const DEFAULT_RUNTIME: &str = "claude";

/// Find a model by exact ID.
pub fn find_model(id: &str) -> Option<ModelEntry> {
    model_catalog().into_iter().find(|m| m.id == id)
}

/// Infer runtime from model ID. Checks catalog first, then prefix patterns.
pub fn runtime_for_model(model_id: &str) -> String {
    if let Some(entry) = find_model(model_id) {
        return entry.runtime;
    }

    let id = model_id.to_lowercase();
    if id.starts_with("claude-") {
        return "claude".into();
    }
    if id.starts_with("gemini-") {
        return "gemini".into();
    }
    if id.starts_with("gpt-")
        || id.starts_with("codex-")
        || id.starts_with("o1-")
        || id.starts_with("o3-")
        || id.starts_with("o4-")
    {
        return "codex".into();
    }

    DEFAULT_RUNTIME.into()
}

/// Resolve a model argument (exact id, number, or substring match).
pub fn resolve_model(args: &str) -> ModelEntry {
    let catalog = model_catalog();
    let lower = args.to_lowercase();

    // Exact match
    if let Some(m) = catalog.iter().find(|m| m.id == lower) {
        return m.clone();
    }

    // Number match
    if let Ok(num) = args.parse::<usize>() {
        if num >= 1 && num <= catalog.len() {
            return catalog[num - 1].clone();
        }
    }

    // Substring match
    if let Some(m) = catalog.iter().find(|m| {
        m.id.contains(&lower) || m.display_name.to_lowercase().contains(&lower)
    }) {
        return m.clone();
    }

    // Accept arbitrary model ID — infer runtime from prefix
    ModelEntry {
        id: lower.clone(),
        runtime: runtime_for_model(&lower),
        display_name: args.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Command result
// ---------------------------------------------------------------------------

/// Side effects that the caller should apply after handling a command.
/// Keeps command handlers pure and testable — no async, no shared state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommandEffect {
    /// Stop the active container for this group.
    KillContainer,
    /// Delete the session for this group (both in-memory and Postgres).
    ClearSession,
    /// Switch the group to a new model and runtime.
    SwitchModel {
        model_id: String,
        runtime: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
    /// Side effects to apply. Empty for read-only commands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<CommandEffect>,
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// Context passed to command handlers.
pub struct CommandContext {
    pub assistant_name: String,
    pub started_at: Instant,
}

pub fn handle_command(
    command: &str,
    args: &str,
    group_name: Option<&str>,
    group_folder: Option<&str>,
    current_model: Option<&str>,
    session_id: Option<&str>,
    container_active: bool,
    ctx: &CommandContext,
) -> CommandResult {
    match command {
        "help" => handle_help(&ctx.assistant_name),
        "status" => handle_status(
            group_name,
            group_folder,
            current_model,
            session_id,
            container_active,
            ctx,
        ),
        "model" => handle_model(args, current_model, group_name),
        "reset" | "new" => handle_reset(group_name, container_active),
        _ => CommandResult {
            text: format!("Unknown command: /{command}"),
            parse_mode: None,
            effects: vec![],
        },
    }
}

fn handle_help(assistant_name: &str) -> CommandResult {
    CommandResult {
        text: format!(
            "*{assistant_name} Commands*\n\
             \n\
             /help — Show this command list\n\
             /status — Show runtime, session, and container status\n\
             /model — Show available models\n\
             /model <#> — Switch model by number\n\
             /model <name> — Switch model by name\n\
             /reset — Clear session and stop running container\n\
             /new — Start a fresh chat (alias for /reset)\n\
             /ping — Check if bot is online\n\
             /chatid — Show this chat's registration ID"
        ),
        parse_mode: Some("Markdown".into()),
        effects: vec![],
    }
}

fn handle_status(
    group_name: Option<&str>,
    group_folder: Option<&str>,
    current_model: Option<&str>,
    session_id: Option<&str>,
    container_active: bool,
    ctx: &CommandContext,
) -> CommandResult {
    let name = group_name.unwrap_or("Unknown");
    if group_folder.is_none() {
        return CommandResult {
            text: "This chat is not registered.".into(),
            parse_mode: None,
            effects: vec![],
        };
    }

    let model_id = current_model.unwrap_or(DEFAULT_MODEL);
    let model_display = find_model(model_id)
        .map(|m| m.display_name)
        .unwrap_or_else(|| model_id.to_string());

    let session_display = match session_id {
        Some(sid) if sid.len() > 12 => format!("`{}...`", &sid[..12]),
        Some(sid) => format!("`{sid}`"),
        None => "_none_".into(),
    };

    let elapsed = ctx.started_at.elapsed();
    let total_min = elapsed.as_secs() / 60;
    let hours = total_min / 60;
    let minutes = total_min % 60;
    let uptime = if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    };

    let container_status = if container_active { "active" } else { "idle" };

    CommandResult {
        text: format!(
            "*Status for {name}*\n\
             \n\
             Model: `{model_display}`\n\
             Session: {session_display}\n\
             Container: {container_status}\n\
             Assistant: {}\n\
             Uptime: {uptime}",
            ctx.assistant_name
        ),
        parse_mode: Some("Markdown".into()),
        effects: vec![],
    }
}

fn handle_model(
    args: &str,
    current_model: Option<&str>,
    group_name: Option<&str>,
) -> CommandResult {
    if group_name.is_none() {
        return CommandResult {
            text: "This chat is not registered.".into(),
            parse_mode: None,
            effects: vec![],
        };
    }

    let current_id = current_model.unwrap_or(DEFAULT_MODEL);

    // No args — show catalog
    if args.is_empty() {
        let current_display = find_model(current_id)
            .map(|m| m.display_name)
            .unwrap_or_else(|| current_id.to_string());

        let catalog = model_catalog();
        let catalog_lines: Vec<String> = catalog
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let active = if m.id == current_id { " (active)" } else { "" };
                format!(" {}. `{}` — {}{}", i + 1, m.id, m.display_name, active)
            })
            .collect();

        return CommandResult {
            text: format!(
                "*Current model:* {current_display}\n\
                 \n\
                 {}\n\
                 \n\
                 Switch: `/model <name>` or `/model <#>`",
                catalog_lines.join("\n")
            ),
            parse_mode: Some("Markdown".into()),
            effects: vec![],
        };
    }

    // Resolve model
    let new_model = resolve_model(args);

    if new_model.id == current_id {
        return CommandResult {
            text: format!("Already using `{}`.", new_model.display_name),
            parse_mode: Some("Markdown".into()),
            effects: vec![],
        };
    }

    let prev_display = find_model(current_id)
        .map(|m| m.display_name)
        .unwrap_or_else(|| current_id.to_string());

    CommandResult {
        text: format!(
            "Switched from {prev_display} to *{}*.\n\
             Conversation context will carry over.",
            new_model.display_name
        ),
        parse_mode: Some("Markdown".into()),
        effects: vec![
            CommandEffect::KillContainer,
            CommandEffect::ClearSession,
            CommandEffect::SwitchModel {
                model_id: new_model.id,
                runtime: new_model.runtime,
            },
        ],
    }
}

fn handle_reset(group_name: Option<&str>, was_active: bool) -> CommandResult {
    if group_name.is_none() {
        return CommandResult {
            text: "This chat is not registered.".into(),
            parse_mode: None,
            effects: vec![],
        };
    }

    let mut parts = vec!["Session cleared.".to_string()];
    if was_active {
        parts.push("Running container stopped.".to_string());
    }
    parts.push("Next message will start a fresh session.".to_string());

    let mut effects = vec![CommandEffect::ClearSession];
    if was_active {
        effects.insert(0, CommandEffect::KillContainer);
    }

    CommandResult {
        text: parts.join(" "),
        parse_mode: None,
        effects,
    }
}

// ---------------------------------------------------------------------------
// HTTP endpoint for commands
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CommandRequest {
    pub chat_jid: String,
    pub command: String,
    #[serde(default)]
    pub args: String,
    pub group_name: Option<String>,
    pub group_folder: Option<String>,
    pub current_model: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub container_active: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> CommandContext {
        CommandContext {
            assistant_name: "TestBot".into(),
            started_at: Instant::now(),
        }
    }

    #[test]
    fn help_command() {
        let result = handle_command("help", "", None, None, None, None, false, &test_ctx());
        assert!(result.text.contains("TestBot Commands"));
        assert_eq!(result.parse_mode, Some("Markdown".into()));
    }

    #[test]
    fn status_unregistered() {
        let result = handle_command("status", "", None, None, None, None, false, &test_ctx());
        assert!(result.text.contains("not registered"));
    }

    #[test]
    fn status_registered() {
        let result = handle_command(
            "status",
            "",
            Some("Test Group"),
            Some("test-group"),
            Some("claude-opus-4-6"),
            Some("sess-abc123def456"),
            true,
            &test_ctx(),
        );
        assert!(result.text.contains("Test Group"));
        assert!(result.text.contains("Claude Opus 4.6"));
        assert!(result.text.contains("active"));
        assert!(result.text.contains("sess-abc123d"));
    }

    #[test]
    fn model_catalog_display() {
        let result = handle_command(
            "model",
            "",
            Some("Test"),
            Some("test"),
            Some("claude-opus-4-6"),
            None,
            false,
            &test_ctx(),
        );
        assert!(result.text.contains("Claude Opus 4.6"));
        assert!(result.text.contains("(active)"));
        assert!(result.text.contains("Gemini"));
    }

    #[test]
    fn model_switch_by_number() {
        let model = resolve_model("2");
        assert_eq!(model.id, "claude-sonnet-4-6");
    }

    #[test]
    fn model_switch_by_name() {
        let model = resolve_model("gemini-3.1-pro");
        assert_eq!(model.id, "gemini-3.1-pro");
        assert_eq!(model.runtime, "gemini");
    }

    #[test]
    fn model_switch_substring() {
        let model = resolve_model("codex");
        assert_eq!(model.id, "gpt-5.3-codex");
    }

    #[test]
    fn model_unknown_infers_runtime() {
        let model = resolve_model("claude-haiku-4-5");
        assert_eq!(model.runtime, "claude");
        assert_eq!(model.id, "claude-haiku-4-5");
    }

    #[test]
    fn model_already_active() {
        let result = handle_command(
            "model",
            "claude-opus-4-6",
            Some("Test"),
            Some("test"),
            Some("claude-opus-4-6"),
            None,
            false,
            &test_ctx(),
        );
        assert!(result.text.contains("Already using"));
    }

    #[test]
    fn reset_with_active_container() {
        let result = handle_command(
            "reset",
            "",
            Some("Test"),
            Some("test"),
            None,
            None,
            true,
            &test_ctx(),
        );
        assert!(result.text.contains("Session cleared"));
        assert!(result.text.contains("container stopped"));
    }

    #[test]
    fn reset_without_active_container() {
        let result = handle_command(
            "reset",
            "",
            Some("Test"),
            Some("test"),
            None,
            None,
            false,
            &test_ctx(),
        );
        assert!(result.text.contains("Session cleared"));
        assert!(!result.text.contains("container stopped"));
    }

    #[test]
    fn new_is_alias_for_reset() {
        let result = handle_command(
            "new",
            "",
            Some("Test"),
            Some("test"),
            None,
            None,
            false,
            &test_ctx(),
        );
        assert!(result.text.contains("Session cleared"));
    }

    #[test]
    fn unknown_command() {
        let result = handle_command("foo", "", None, None, None, None, false, &test_ctx());
        assert!(result.text.contains("Unknown command: /foo"));
    }

    #[test]
    fn runtime_for_model_prefix_inference() {
        assert_eq!(runtime_for_model("claude-anything"), "claude");
        assert_eq!(runtime_for_model("gemini-anything"), "gemini");
        assert_eq!(runtime_for_model("gpt-anything"), "codex");
        assert_eq!(runtime_for_model("o4-mini"), "codex");
        assert_eq!(runtime_for_model("unknown-model"), DEFAULT_RUNTIME);
    }

    #[test]
    fn find_model_exact() {
        let m = find_model("claude-opus-4-6");
        assert!(m.is_some());
        assert_eq!(m.unwrap().display_name, "Claude Opus 4.6");
    }

    #[test]
    fn find_model_missing() {
        assert!(find_model("nonexistent").is_none());
    }

    // --- Effects tests ---

    #[test]
    fn reset_effects_with_active_container() {
        let result = handle_command(
            "reset", "", Some("Test"), Some("test"), None, None, true, &test_ctx(),
        );
        assert_eq!(result.effects, vec![
            CommandEffect::KillContainer,
            CommandEffect::ClearSession,
        ]);
    }

    #[test]
    fn reset_effects_without_active_container() {
        let result = handle_command(
            "reset", "", Some("Test"), Some("test"), None, None, false, &test_ctx(),
        );
        assert_eq!(result.effects, vec![CommandEffect::ClearSession]);
    }

    #[test]
    fn model_switch_effects() {
        let result = handle_command(
            "model", "gemini-3.1-pro",
            Some("Test"), Some("test"), Some("claude-opus-4-6"), None, false,
            &test_ctx(),
        );
        assert_eq!(result.effects, vec![
            CommandEffect::KillContainer,
            CommandEffect::ClearSession,
            CommandEffect::SwitchModel {
                model_id: "gemini-3.1-pro".into(),
                runtime: "gemini".into(),
            },
        ]);
    }

    #[test]
    fn model_already_active_no_effects() {
        let result = handle_command(
            "model", "claude-opus-4-6",
            Some("Test"), Some("test"), Some("claude-opus-4-6"), None, false,
            &test_ctx(),
        );
        assert!(result.effects.is_empty());
    }

    #[test]
    fn help_no_effects() {
        let result = handle_command("help", "", None, None, None, None, false, &test_ctx());
        assert!(result.effects.is_empty());
    }

    #[test]
    fn status_no_effects() {
        let result = handle_command(
            "status", "", Some("Test"), Some("test"), Some("claude-opus-4-6"), None, true,
            &test_ctx(),
        );
        assert!(result.effects.is_empty());
    }

    #[test]
    fn unregistered_group_no_effects() {
        let result = handle_command("reset", "", None, None, None, None, false, &test_ctx());
        assert!(result.effects.is_empty());
    }
}
