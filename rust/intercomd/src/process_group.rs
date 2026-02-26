//! processGroupMessages callback — invoked by GroupQueue when it's a group's turn.
//!
//! Port of `processGroupMessages()` + `runAgent()` from `src/index.ts`.
//!
//! Flow:
//! 1. Look up group from JID in shared state
//! 2. Fetch pending messages from Postgres since lastAgentTimestamp
//! 3. Check trigger for non-main groups
//! 4. Format prompt from messages
//! 5. Spawn container via run_container_agent()
//! 6. Stream output: route results to Telegram
//! 7. Store bot responses in Postgres
//! 8. Advance per-group cursor on success, rollback on error

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use intercom_core::{
    ContainerInput, ContainerOutput, ContainerStatus, PgPool, RegisteredGroup, RuntimeKind,
};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::container::mounts::GroupInfo;
use crate::container::runner::{OutputCallback, RunConfig, run_container_agent};
use crate::container::security::ContainerConfig;
use crate::message_loop::{self, AgentTimestamps};
use crate::queue::{GroupQueue, ProcessMessagesFn};
use crate::telegram::TelegramBridge;

/// Build the `ProcessMessagesFn` closure that GroupQueue invokes for message processing.
///
/// The returned closure captures all shared state and is `Send + Sync`.
pub fn build_process_messages_fn(
    pool: PgPool,
    queue: Arc<GroupQueue>,
    groups: Arc<RwLock<HashMap<String, RegisteredGroup>>>,
    sessions: Arc<RwLock<HashMap<String, String>>>,
    telegram: Arc<TelegramBridge>,
    assistant_name: String,
    main_group_folder: String,
    run_config: RunConfig,
) -> ProcessMessagesFn {
    Arc::new(move |chat_jid: String| {
        let pool = pool.clone();
        let queue = queue.clone();
        let groups = groups.clone();
        let sessions = sessions.clone();
        let telegram = telegram.clone();
        let assistant_name = assistant_name.clone();
        let main_group_folder = main_group_folder.clone();
        let run_config = run_config.clone();

        Box::pin(async move {
            match process_group_messages(
                &chat_jid,
                &pool,
                &queue,
                &groups,
                &sessions,
                &telegram,
                &assistant_name,
                &main_group_folder,
                &run_config,
            )
            .await
            {
                Ok(success) => success,
                Err(e) => {
                    error!(chat_jid, err = %e, "processGroupMessages failed");
                    false
                }
            }
        })
    })
}

/// Core logic for processing messages for a single group.
async fn process_group_messages(
    chat_jid: &str,
    pool: &PgPool,
    queue: &Arc<GroupQueue>,
    groups: &Arc<RwLock<HashMap<String, RegisteredGroup>>>,
    sessions: &Arc<RwLock<HashMap<String, String>>>,
    telegram: &Arc<TelegramBridge>,
    assistant_name: &str,
    main_group_folder: &str,
    run_config: &RunConfig,
) -> anyhow::Result<bool> {
    // 1. Look up group
    let group = {
        let g = groups.read().await;
        match g.get(chat_jid) {
            Some(group) => group.clone(),
            None => return Ok(true), // unknown group — skip, not an error
        }
    };

    let is_main = group.folder == main_group_folder;

    // 2. Load agent timestamp and fetch pending messages
    let mut agent_timestamps = message_loop::load_agent_timestamps_pub(pool).await;
    let since = agent_timestamps
        .0
        .get(chat_jid)
        .cloned()
        .unwrap_or_default();

    let pending = pool
        .get_messages_since(chat_jid, &since, assistant_name)
        .await?;

    if pending.is_empty() {
        return Ok(true);
    }

    // 3. Check trigger for non-main groups
    if !is_main && group.requires_trigger.unwrap_or(true) {
        let trigger = if group.trigger.is_empty() {
            None
        } else {
            Some(group.trigger.as_str())
        };
        let re = message_loop::build_trigger_regex_pub(assistant_name, trigger);
        let has_trigger = pending.iter().any(|m| re.is_match(m.content.trim()));
        if !has_trigger {
            return Ok(true);
        }
    }

    // 4. Format prompt
    let prompt = message_loop::format_messages_pub(&pending);

    // Save cursor position for rollback on error
    let previous_cursor = since.clone();
    let new_cursor = pending
        .last()
        .map(|m| m.timestamp.clone())
        .unwrap_or_default();

    // Advance cursor before running agent (matches Node behavior)
    agent_timestamps
        .0
        .insert(chat_jid.to_string(), new_cursor.clone());
    message_loop::save_agent_timestamps_pub(pool, &agent_timestamps).await;

    info!(
        group = group.name.as_str(),
        message_count = pending.len(),
        "processing messages"
    );

    // 5. Resolve runtime and session
    let runtime = resolve_runtime(&group);
    let session_id = {
        let s = sessions.read().await;
        s.get(&group.folder).cloned()
    };

    let input = ContainerInput {
        prompt,
        session_id,
        group_folder: group.folder.clone(),
        chat_jid: chat_jid.to_string(),
        is_main,
        is_scheduled_task: None,
        assistant_name: Some(assistant_name.to_string()),
        model: group.model.clone(),
        secrets: None, // Secrets injected by runner from env files
    };

    let group_info = GroupInfo {
        folder: group.folder.clone(),
        name: group.name.clone(),
        container_config: group
            .container_config
            .as_ref()
            .and_then(|v| serde_json::from_value::<ContainerConfig>(v.clone()).ok()),
    };

    // 6. Run container and collect output
    let sessions_clone: Arc<RwLock<HashMap<String, String>>> = sessions.clone();
    let group_folder = group.folder.clone();
    let queue_clone: Arc<GroupQueue> = queue.clone();
    let chat_jid_owned = chat_jid.to_string();

    // Track whether we sent any output to the user
    let output_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_sent_cb = output_sent.clone();

    let telegram_cb: Arc<TelegramBridge> = telegram.clone();
    let pool_cb = pool.clone();
    let assistant_name_cb = assistant_name.to_string();

    let on_output: Option<Arc<OutputCallback>> = Some(Arc::new(Box::new(
        move |output: ContainerOutput| {
            let sessions = sessions_clone.clone();
            let group_folder = group_folder.clone();
            let queue = queue_clone.clone();
            let chat_jid = chat_jid_owned.clone();
            let telegram = telegram_cb.clone();
            let pool = pool_cb.clone();
            let assistant_name = assistant_name_cb.clone();
            let output_sent = output_sent_cb.clone();

            Box::pin(async move {
                // Track session ID from container
                if let Some(ref sid) = output.new_session_id {
                    let mut s = sessions.write().await;
                    s.insert(group_folder.clone(), sid.clone());
                    // Persist to Postgres
                    if let Err(e) = pool.set_session(&group_folder, sid).await {
                        warn!(err = %e, "failed to persist session");
                    }
                }

                // Handle final result
                if let Some(ref result_text) = output.result {
                    // Strip <internal>...</internal> blocks
                    let text = strip_internal_blocks(result_text);
                    if !text.is_empty() {
                        // Send via Telegram
                        if let Err(e) = telegram
                            .send_text_to_jid(&chat_jid, &text)
                            .await
                        {
                            error!(err = %e, "failed to send agent output via Telegram");
                        }

                        // Store bot response in Postgres
                        let bot_msg = intercom_core::NewMessage {
                            id: format!("bot-{}", chrono::Utc::now().timestamp_millis()),
                            chat_jid: chat_jid.clone(),
                            sender: "bot".into(),
                            sender_name: assistant_name.clone(),
                            content: text,
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            is_from_me: true,
                            is_bot_message: true,
                        };
                        if let Err(e) = pool.store_message(&bot_msg).await {
                            warn!(err = %e, "failed to store bot response");
                        }

                        output_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }

                // Notify queue on completion
                if output.status == ContainerStatus::Success {
                    queue.notify_idle(&chat_jid).await;
                }
            })
        },
    )));

    let result = run_container_agent(
        &group_info,
        &input,
        runtime,
        is_main,
        run_config,
        on_output,
    )
    .await;

    // 7. Handle result
    match result {
        Ok(run_result) => {
            // Track session from final output
            if let Some(ref sid) = run_result.output.new_session_id {
                let mut s = sessions.write().await;
                s.insert(group.folder.clone(), sid.clone());
                if let Err(e) = pool.set_session(&group.folder, sid).await {
                    warn!(err = %e, "failed to persist session");
                }
            }

            if run_result.output.status == ContainerStatus::Error {
                // Error, but if we already sent output, don't rollback cursor
                if output_sent.load(std::sync::atomic::Ordering::SeqCst) {
                    warn!(
                        group = group.name.as_str(),
                        "agent error after output sent, skipping cursor rollback"
                    );
                    return Ok(true);
                }

                // Rollback cursor for retry
                agent_timestamps
                    .0
                    .insert(chat_jid.to_string(), previous_cursor);
                message_loop::save_agent_timestamps_pub(pool, &agent_timestamps).await;
                warn!(
                    group = group.name.as_str(),
                    "agent error, rolled back cursor for retry"
                );
                return Ok(false);
            }

            Ok(true)
        }
        Err(e) => {
            error!(group = group.name.as_str(), err = %e, "container agent error");

            if output_sent.load(std::sync::atomic::Ordering::SeqCst) {
                warn!(
                    group = group.name.as_str(),
                    "agent error after output sent, skipping cursor rollback"
                );
                return Ok(true);
            }

            // Rollback cursor
            agent_timestamps
                .0
                .insert(chat_jid.to_string(), previous_cursor);
            message_loop::save_agent_timestamps_pub(pool, &agent_timestamps).await;
            Ok(false)
        }
    }
}

/// Resolve runtime kind from group configuration.
pub(crate) fn resolve_runtime(group: &RegisteredGroup) -> RuntimeKind {
    match group.runtime.as_deref() {
        Some("gemini") => RuntimeKind::Gemini,
        Some("codex") => RuntimeKind::Codex,
        _ => RuntimeKind::Claude, // default
    }
}

/// Strip `<internal>...</internal>` blocks from agent output.
fn strip_internal_blocks(text: &str) -> String {
    // Simple regex-free approach: find and remove <internal>...</internal> spans
    let mut result = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("<internal>") {
        result.push_str(&rest[..start]);
        if let Some(end) = rest[start..].find("</internal>") {
            rest = &rest[start + end + "</internal>".len()..];
        } else {
            // Unclosed tag — strip to end
            rest = "";
            break;
        }
    }
    result.push_str(rest);
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_internal_basic() {
        let input = "Hello <internal>reasoning here</internal> World";
        assert_eq!(strip_internal_blocks(input), "Hello  World");
    }

    #[test]
    fn strip_internal_multiple() {
        let input = "A <internal>x</internal> B <internal>y</internal> C";
        assert_eq!(strip_internal_blocks(input), "A  B  C");
    }

    #[test]
    fn strip_internal_none() {
        assert_eq!(strip_internal_blocks("Hello World"), "Hello World");
    }

    #[test]
    fn strip_internal_unclosed() {
        let input = "Hello <internal>never closed";
        assert_eq!(strip_internal_blocks(input), "Hello");
    }

    #[test]
    fn strip_internal_multiline() {
        let input = "Before\n<internal>\nmulti\nline\n</internal>\nAfter";
        assert_eq!(strip_internal_blocks(input), "Before\n\nAfter");
    }

    #[test]
    fn resolve_runtime_defaults_to_claude() {
        let group = RegisteredGroup {
            jid: "tg:123".into(),
            name: "Test".into(),
            folder: "test".into(),
            trigger: String::new(),
            added_at: String::new(),
            container_config: None,
            requires_trigger: None,
            runtime: None,
            model: None,
        };
        assert_eq!(resolve_runtime(&group), RuntimeKind::Claude);
    }

    #[test]
    fn resolve_runtime_gemini() {
        let group = RegisteredGroup {
            jid: "tg:123".into(),
            name: "Test".into(),
            folder: "test".into(),
            trigger: String::new(),
            added_at: String::new(),
            container_config: None,
            requires_trigger: None,
            runtime: Some("gemini".into()),
            model: None,
        };
        assert_eq!(resolve_runtime(&group), RuntimeKind::Gemini);
    }
}
