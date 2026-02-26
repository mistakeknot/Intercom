//! Message poll loop — polls Postgres for new messages and dispatches to GroupQueue.
//!
//! Port of `startMessageLoop()` from `src/index.ts`.
//!
//! Dual-cursor design:
//! - `last_timestamp` (global): advances when ANY messages are fetched. Prevents re-fetching.
//! - `last_agent_timestamp` (per-group): advances when messages are dispatched to a container.
//!   Accumulated context between triggers is preserved.
//!
//! On startup, `recover_pending_messages()` re-enqueues groups with unprocessed messages
//! (handles crash between advancing last_timestamp and agent dispatch).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use intercom_core::{PgPool, RegisteredGroup};
use regex::Regex;
use tokio::sync::{RwLock, watch};
use tracing::{debug, error, info, warn};

use crate::queue::GroupQueue;

/// Configuration for the message loop.
#[derive(Debug, Clone)]
pub struct MessageLoopConfig {
    /// Poll interval (milliseconds).
    pub poll_interval_ms: u64,
    /// Bot/assistant name prefix used to filter out bot messages and detect triggers.
    pub assistant_name: String,
    /// Folder name for the main group (e.g., "main"). Main group doesn't require trigger.
    pub main_group_folder: String,
}

/// Per-group cursor state. Stored in router_state as JSON.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct AgentTimestamps(pub HashMap<String, String>);

/// Run the message poll loop. Exits when shutdown signal fires.
pub async fn run_message_loop(
    config: MessageLoopConfig,
    pool: PgPool,
    queue: Arc<GroupQueue>,
    groups: Arc<RwLock<HashMap<String, RegisteredGroup>>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let interval = Duration::from_millis(config.poll_interval_ms);

    // Load cursor state from Postgres
    let mut last_timestamp = load_cursor(&pool, "last_timestamp").await;
    let mut agent_timestamps = load_agent_timestamps(&pool).await;

    info!(
        poll_interval_ms = config.poll_interval_ms,
        last_timestamp = %last_timestamp,
        agent_cursors = agent_timestamps.0.len(),
        "message loop started"
    );

    // Run recovery before entering the main loop
    recover_pending_messages(
        &pool,
        &queue,
        &groups,
        &agent_timestamps,
        &config.assistant_name,
        &config.main_group_folder,
    )
    .await;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("message loop shutting down");
                    return;
                }
            }
        }

        if let Err(e) = poll_once(
            &config,
            &pool,
            &queue,
            &groups,
            &mut last_timestamp,
            &mut agent_timestamps,
        )
        .await
        {
            error!(err = %e, "error in message poll");
        }
    }
}

/// Single poll iteration. Extracted for testability.
async fn poll_once(
    config: &MessageLoopConfig,
    pool: &PgPool,
    queue: &GroupQueue,
    groups: &RwLock<HashMap<String, RegisteredGroup>>,
    last_timestamp: &mut String,
    agent_timestamps: &mut AgentTimestamps,
) -> anyhow::Result<()> {
    let groups_guard = groups.read().await;
    let jids: Vec<String> = groups_guard.keys().cloned().collect();
    drop(groups_guard);

    if jids.is_empty() {
        return Ok(());
    }

    let (messages, new_timestamp) = pool
        .get_new_messages(&jids, last_timestamp, &config.assistant_name)
        .await?;

    if messages.is_empty() {
        return Ok(());
    }

    info!(count = messages.len(), "new messages");

    // Advance the global "seen" cursor immediately
    *last_timestamp = new_timestamp;
    save_cursor(pool, "last_timestamp", last_timestamp).await;

    // Group messages by chat JID
    let mut by_group: HashMap<String, Vec<intercom_core::NewMessage>> = HashMap::new();
    for msg in messages {
        by_group
            .entry(msg.chat_jid.clone())
            .or_default()
            .push(msg);
    }

    let groups_guard = groups.read().await;

    for (chat_jid, group_messages) in by_group {
        let group = match groups_guard.get(&chat_jid) {
            Some(g) => g,
            None => continue,
        };

        let is_main = group.folder == config.main_group_folder;
        let needs_trigger = !is_main && group.requires_trigger.unwrap_or(true);

        // For non-main groups, only act on trigger messages.
        // Non-trigger messages accumulate in DB; they'll be pulled as context
        // when a trigger eventually arrives.
        if needs_trigger {
            let trigger_pattern = build_trigger_regex(&config.assistant_name, if group.trigger.is_empty() { None } else { Some(group.trigger.as_str()) });
            let has_trigger = group_messages
                .iter()
                .any(|m| trigger_pattern.is_match(m.content.trim()));
            if !has_trigger {
                continue;
            }
        }

        // Try to pipe to active container first
        let agent_since = agent_timestamps
            .0
            .get(&chat_jid)
            .cloned()
            .unwrap_or_default();

        // Pull ALL messages since last agent timestamp (includes accumulated context)
        let all_pending = pool
            .get_messages_since(&chat_jid, &agent_since, &config.assistant_name)
            .await
            .unwrap_or_default();

        let messages_to_use = if all_pending.is_empty() {
            &group_messages
        } else {
            &all_pending
        };

        let formatted = format_messages(messages_to_use);

        if queue.send_message(&chat_jid, &formatted).await {
            debug!(
                chat_jid = chat_jid.as_str(),
                count = messages_to_use.len(),
                "piped messages to active container"
            );
            // Advance per-group cursor
            if let Some(last) = messages_to_use.last() {
                agent_timestamps
                    .0
                    .insert(chat_jid.clone(), last.timestamp.clone());
                save_agent_timestamps(pool, &agent_timestamps).await;
            }
        } else {
            // No active container — enqueue for processing
            queue.enqueue_message_check(&chat_jid).await;
        }
    }

    Ok(())
}

/// Startup recovery: check for unprocessed messages in registered groups.
async fn recover_pending_messages(
    pool: &PgPool,
    queue: &GroupQueue,
    groups: &RwLock<HashMap<String, RegisteredGroup>>,
    agent_timestamps: &AgentTimestamps,
    assistant_name: &str,
    main_group_folder: &str,
) {
    let groups_guard = groups.read().await;
    for (chat_jid, group) in groups_guard.iter() {
        let since = agent_timestamps
            .0
            .get(chat_jid)
            .cloned()
            .unwrap_or_default();
        let pending = match pool
            .get_messages_since(chat_jid, &since, assistant_name)
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                warn!(chat_jid, err = %e, "recovery: failed to check pending messages");
                continue;
            }
        };

        if !pending.is_empty() {
            let is_main = group.folder == main_group_folder;
            let needs_trigger = !is_main && group.requires_trigger.unwrap_or(true);

            if needs_trigger {
                let trigger_pattern = build_trigger_regex(assistant_name, if group.trigger.is_empty() { None } else { Some(group.trigger.as_str()) });
                let has_trigger = pending.iter().any(|m| trigger_pattern.is_match(m.content.trim()));
                if !has_trigger {
                    continue;
                }
            }

            info!(
                group = group.name.as_str(),
                pending_count = pending.len(),
                "recovery: enqueuing unprocessed messages"
            );
            queue.enqueue_message_check(chat_jid).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor persistence
// ---------------------------------------------------------------------------

async fn load_cursor(pool: &PgPool, key: &str) -> String {
    match pool.get_router_state(key).await {
        Ok(Some(v)) => v,
        Ok(None) => String::new(),
        Err(e) => {
            warn!(key, err = %e, "failed to load cursor, starting from empty");
            String::new()
        }
    }
}

async fn save_cursor(pool: &PgPool, key: &str, value: &str) {
    if let Err(e) = pool.set_router_state(key, value).await {
        error!(key, err = %e, "failed to save cursor");
    }
}

/// Public wrapper for loading agent timestamps (used by process_group).
pub async fn load_agent_timestamps_pub(pool: &PgPool) -> AgentTimestamps {
    load_agent_timestamps(pool).await
}

/// Public wrapper for saving agent timestamps (used by process_group).
pub async fn save_agent_timestamps_pub(pool: &PgPool, timestamps: &AgentTimestamps) {
    save_agent_timestamps(pool, timestamps).await;
}

/// Public wrapper for building trigger regex (used by process_group).
pub fn build_trigger_regex_pub(assistant_name: &str, custom_trigger: Option<&str>) -> regex::Regex {
    build_trigger_regex(assistant_name, custom_trigger)
}

/// Public wrapper for formatting messages (used by process_group).
pub fn format_messages_pub(messages: &[intercom_core::NewMessage]) -> String {
    format_messages(messages)
}

async fn load_agent_timestamps(pool: &PgPool) -> AgentTimestamps {
    match pool.get_router_state("last_agent_timestamp").await {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        Ok(None) => AgentTimestamps::default(),
        Err(e) => {
            warn!(err = %e, "failed to load agent timestamps, starting from empty");
            AgentTimestamps::default()
        }
    }
}

async fn save_agent_timestamps(pool: &PgPool, timestamps: &AgentTimestamps) {
    let json = serde_json::to_string(timestamps).unwrap_or_else(|_| "{}".into());
    if let Err(e) = pool.set_router_state("last_agent_timestamp", &json).await {
        error!(err = %e, "failed to save agent timestamps");
    }
}

// ---------------------------------------------------------------------------
// Message formatting
// ---------------------------------------------------------------------------

/// Format messages into a prompt string for the container agent.
/// Matches the `formatMessages()` function in `src/router.ts`.
fn format_messages(messages: &[intercom_core::NewMessage]) -> String {
    messages
        .iter()
        .map(|m| {
            if m.is_bot_message {
                format!("[{}]: {}", m.sender_name, m.content)
            } else {
                format!("[{}]: {}", m.sender_name, m.content)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a trigger regex that matches `@AssistantName` at word boundary.
/// If the group has a custom trigger, use that as an additional pattern.
fn build_trigger_regex(assistant_name: &str, custom_trigger: Option<&str>) -> Regex {
    let escaped = regex::escape(assistant_name);
    let pattern = if let Some(trigger) = custom_trigger {
        if trigger.is_empty() {
            format!(r"(?i)^@{}\b", escaped)
        } else {
            let escaped_trigger = regex::escape(trigger);
            format!(r"(?i)^@{}\b|^{}\b", escaped, escaped_trigger)
        }
    } else {
        format!(r"(?i)^@{}\b", escaped)
    };

    Regex::new(&pattern).unwrap_or_else(|_| {
        // Fallback to simple prefix match
        Regex::new(&format!(r"(?i)^@{}", regex::escape(assistant_name))).unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_messages_basic() {
        let msgs = vec![
            intercom_core::NewMessage {
                id: "1".into(),
                chat_jid: "tg:123".into(),
                sender: "user1".into(),
                sender_name: "Alice".into(),
                content: "Hello".into(),
                timestamp: "2024-01-15T12:00:00Z".into(),
                is_from_me: false,
                is_bot_message: false,
            },
            intercom_core::NewMessage {
                id: "2".into(),
                chat_jid: "tg:123".into(),
                sender: "bot".into(),
                sender_name: "Amtiskaw".into(),
                content: "Hi there".into(),
                timestamp: "2024-01-15T12:01:00Z".into(),
                is_from_me: true,
                is_bot_message: true,
            },
        ];
        let result = format_messages(&msgs);
        assert!(result.contains("[Alice]: Hello"));
        assert!(result.contains("[Amtiskaw]: Hi there"));
    }

    #[test]
    fn trigger_regex_matches_at_mention() {
        let re = build_trigger_regex("Amtiskaw", None);
        assert!(re.is_match("@Amtiskaw hello"));
        assert!(re.is_match("@amtiskaw hello")); // case insensitive
        assert!(!re.is_match("hello @Amtiskaw")); // not at start
    }

    #[test]
    fn trigger_regex_with_custom() {
        let re = build_trigger_regex("Amtiskaw", Some("!ai"));
        assert!(re.is_match("@Amtiskaw hello"));
        assert!(re.is_match("!ai do something"));
        assert!(!re.is_match("hello !ai")); // not at start
    }

    #[test]
    fn agent_timestamps_serde_roundtrip() {
        let mut ts = AgentTimestamps::default();
        ts.0.insert("tg:123".into(), "2024-01-15T12:00:00Z".into());
        ts.0.insert("tg:456".into(), "2024-01-15T13:00:00Z".into());
        let json = serde_json::to_string(&ts).unwrap();
        let parsed: AgentTimestamps = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.0.len(), 2);
        assert_eq!(parsed.0.get("tg:123").unwrap(), "2024-01-15T12:00:00Z");
    }

    #[test]
    fn format_empty_messages() {
        let result = format_messages(&[]);
        assert!(result.is_empty());
    }
}
