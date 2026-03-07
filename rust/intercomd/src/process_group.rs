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
use std::time::Duration;

use intercom_core::{
    ContainerInput, ContainerOutput, ContainerStatus, PgPool, RegisteredGroup, RuntimeKind,
};
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::container::mounts::GroupInfo;
use crate::container::runner::{OutputCallback, RunConfig, pool_spawn, run_container_agent, write_snapshots};
use crate::container::security::ContainerConfig;
use crate::message_loop::{self, AgentTimestamps};
use crate::queue::{GroupQueue, ProcessMessagesFn};
use crate::telegram::TelegramBridge;

use intercom_core::PoolConfig;

/// Build the `ProcessMessagesFn` closure that GroupQueue invokes for message processing.
///
/// The returned closure captures all shared state and is `Send + Sync`.
pub fn build_process_messages_fn(
    pool: PgPool,
    queue: Arc<GroupQueue>,
    groups: Arc<RwLock<HashMap<String, RegisteredGroup>>>,
    sessions: Arc<RwLock<HashMap<String, String>>>,
    shared_timestamps: Arc<RwLock<AgentTimestamps>>,
    telegram: Arc<TelegramBridge>,
    assistant_name: String,
    main_group_folder: String,
    run_config: RunConfig,
    pool_config: PoolConfig,
) -> ProcessMessagesFn {
    Arc::new(move |chat_jid: String| {
        let pool = pool.clone();
        let queue = queue.clone();
        let groups = groups.clone();
        let sessions = sessions.clone();
        let shared_timestamps = shared_timestamps.clone();
        let telegram = telegram.clone();
        let assistant_name = assistant_name.clone();
        let main_group_folder = main_group_folder.clone();
        let run_config = run_config.clone();
        let pool_config = pool_config.clone();

        Box::pin(async move {
            match process_group_messages(
                &chat_jid,
                &pool,
                &queue,
                &groups,
                &sessions,
                &shared_timestamps,
                &telegram,
                &assistant_name,
                &main_group_folder,
                &run_config,
                &pool_config,
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
    shared_timestamps: &Arc<RwLock<AgentTimestamps>>,
    telegram: &Arc<TelegramBridge>,
    assistant_name: &str,
    main_group_folder: &str,
    run_config: &RunConfig,
    pool_config: &PoolConfig,
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

    // 2. Read agent timestamp from shared state (no Postgres round-trip)
    let since = {
        let ts = shared_timestamps.read().await;
        ts.0.get(chat_jid).cloned().unwrap_or_default()
    };

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
    {
        let mut ts = shared_timestamps.write().await;
        ts.0.insert(chat_jid.to_string(), new_cursor.clone());
        message_loop::save_agent_timestamps_pub(pool, &ts).await;
    }

    info!(
        group = group.name.as_str(),
        message_count = pending.len(),
        "processing messages"
    );

    // 5. Resolve runtime and session
    let runtime = resolve_runtime(&group);
    let mut session_id = {
        let s = sessions.read().await;
        s.get(&group.folder).cloned()
    };

    // 5a. Session size guard: if the session JSONL exceeds the configured max,
    // auto-reset to prevent bloated context from causing result timeouts.
    if let Some(ref sid) = session_id {
        let session_jsonl = run_config
            .data_dir
            .join("sessions")
            .join(&group.folder)
            .join(".claude/projects/-workspace-group")
            .join(format!("{}.jsonl", sid));
        match std::fs::metadata(&session_jsonl) {
            Ok(meta) if meta.len() > run_config.session_max_bytes => {
                warn!(
                    group = group.name.as_str(),
                    session_id = sid.as_str(),
                    file_bytes = meta.len(),
                    max_bytes = run_config.session_max_bytes,
                    "session file exceeds size limit — auto-resetting"
                );
                // Delete the session JSONL and its directory
                let session_dir = session_jsonl.parent().unwrap().join(sid);
                let _ = std::fs::remove_file(&session_jsonl);
                let _ = std::fs::remove_dir_all(&session_dir);
                // Clear from memory and Postgres
                {
                    let mut s = sessions.write().await;
                    s.remove(&group.folder);
                }
                if let Err(e) = pool.delete_session(&group.folder).await {
                    warn!(err = %e, "failed to delete session from Postgres");
                }
                session_id = None;
            }
            _ => {} // File doesn't exist or is under limit — fine
        }
    }

    let prompt_for_ipc = prompt.clone();
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

    // 5b. Write task/group snapshots for container consumption
    {
        let tasks_json = match pool.get_all_tasks().await {
            Ok(tasks) => {
                let filtered: Vec<_> = if is_main {
                    tasks
                } else {
                    tasks.into_iter().filter(|t| t.group_folder == group.folder).collect()
                };
                serde_json::to_string(&filtered).unwrap_or_else(|_| "[]".into())
            }
            Err(e) => {
                warn!(err = %e, "failed to load tasks for snapshot");
                "[]".into()
            }
        };
        let groups_json = {
            let g = groups.read().await;
            let entries: Vec<_> = g.values().map(|rg| serde_json::json!({
                "jid": rg.jid,
                "name": rg.name,
                "folder": rg.folder,
            })).collect();
            serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into())
        };
        write_snapshots(&run_config.data_dir, &group.folder, is_main, &tasks_json, &groups_json).await;
    }

    // Track whether we sent any output to the user
    let output_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let output_sent_cb = output_sent.clone();

    // 5c. Check for warm container (pool path)
    if pool_config.enabled && queue.has_warm_container(chat_jid).await {
        info!(group = group.name.as_str(), "using warm container for message delivery");

        // Send message via IPC and get delivery channel
        if let Some(mut delivery_rx) = queue.send_to_warm_container(chat_jid, &prompt_for_ipc).await {
            // Start typing indicator
            let is_telegram = chat_jid.starts_with("tg:");
            if is_telegram {
                telegram.send_typing(chat_jid).await;
            }
            let typing_cancel = Arc::new(Notify::new());
            let typing_handle: Option<JoinHandle<()>> = if is_telegram {
                let tg = telegram.clone();
                let jid = chat_jid.to_string();
                let cancel = typing_cancel.clone();
                Some(tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(4));
                    interval.tick().await;
                    loop {
                        tokio::select! {
                            _ = interval.tick() => { tg.send_typing(&jid).await; }
                            _ = cancel.notified() => { break; }
                        }
                    }
                }))
            } else {
                None
            };

            // Process output frames from the delivery channel
            let sessions_clone = sessions.clone();
            let group_folder = group.folder.clone();
            let pool_cb = pool.clone();
            let queue_cb = queue.clone();
            let chat_jid_owned = chat_jid.to_string();

            while let Some(output) = delivery_rx.recv().await {
                // Track session ID
                if let Some(ref sid) = output.new_session_id {
                    let mut s = sessions_clone.write().await;
                    s.insert(group_folder.clone(), sid.clone());
                    if let Err(e) = pool_cb.set_session(&group_folder, sid).await {
                        warn!(err = %e, "failed to persist session");
                    }
                }

                // Handle final result
                if let Some(ref result_text) = output.result {
                    let text = strip_internal_blocks(result_text);
                    if !text.is_empty() {
                        // Persist before deliver
                        let bot_msg = intercom_core::NewMessage {
                            id: format!("bot-{}", chrono::Utc::now().timestamp_millis()),
                            chat_jid: chat_jid_owned.clone(),
                            sender: "bot".into(),
                            sender_name: assistant_name.to_string(),
                            content: text.clone(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            is_from_me: true,
                            is_bot_message: true,
                        };
                        if let Err(e) = pool_cb.store_message(&bot_msg).await {
                            warn!(err = %e, "failed to store bot response (warm)");
                        }

                        typing_cancel.notify_one();
                        if let Err(e) = telegram.send_text_to_jid(&chat_jid_owned, &text).await {
                            error!(err = %e, "failed to send agent output via Telegram (warm)");
                        }
                        output_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }

                // Container signals idle — this message is done
                if output.status == ContainerStatus::Success && output.result.is_none() {
                    queue_cb.notify_idle(&chat_jid_owned).await;
                    break;
                }
            }

            typing_cancel.notify_one();
            if let Some(handle) = typing_handle {
                handle.abort();
            }

            return Ok(true);
        }
        // If send_to_warm_container returned None, fall through to cold spawn
        warn!(group = group.name.as_str(), "warm container send failed, falling back to cold spawn");
    }

    // 5e. Pool spawn path: if pool is enabled and this is the first message,
    // spawn via pool_spawn() instead of run_container_agent() so the container
    // stays alive for subsequent messages.
    if pool_config.enabled {
        info!(group = group.name.as_str(), "spawning pool container (first message, cold start)");

        // Start typing indicator
        let is_telegram = chat_jid.starts_with("tg:");
        if is_telegram {
            telegram.send_typing(chat_jid).await;
        }
        let typing_cancel = Arc::new(Notify::new());
        let typing_handle: Option<JoinHandle<()>> = if is_telegram {
            let tg = telegram.clone();
            let jid = chat_jid.to_string();
            let cancel = typing_cancel.clone();
            Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(4));
                interval.tick().await;
                loop {
                    tokio::select! {
                        _ = interval.tick() => { tg.send_typing(&jid).await; }
                        _ = cancel.notified() => { break; }
                    }
                }
            }))
        } else {
            None
        };

        // Write task/group snapshots before spawning
        {
            let tasks_json = match pool.get_all_tasks().await {
                Ok(tasks) => {
                    let filtered: Vec<_> = if is_main {
                        tasks
                    } else {
                        tasks.into_iter().filter(|t| t.group_folder == group.folder).collect()
                    };
                    serde_json::to_string(&filtered).unwrap_or_else(|_| "[]".into())
                }
                Err(e) => {
                    warn!(err = %e, "failed to load tasks for snapshot");
                    "[]".into()
                }
            };
            let groups_json = {
                let g = groups.read().await;
                let entries: Vec<_> = g.values().map(|rg| serde_json::json!({
                    "jid": rg.jid,
                    "name": rg.name,
                    "folder": rg.folder,
                })).collect();
                serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into())
            };
            write_snapshots(&run_config.data_dir, &group.folder, is_main, &tasks_json, &groups_json).await;
        }

        match pool_spawn(&group_info, &input, runtime, is_main, run_config).await {
            Ok((mut pc, mut delivery_rx)) => {
                let container_name = pc.container_name.clone();

                // Register the process in the queue for IPC features
                queue.register_process(chat_jid, &container_name, Some(group.folder.as_str())).await;

                // Set up exit monitor that clears pool_container on crash
                let exit_queue = queue.clone();
                let exit_jid = chat_jid.to_string();
                let exit_folder = group.folder.clone();
                let exit_name = container_name.clone();
                pc.exit_monitor.abort(); // Cancel the placeholder
                pc.exit_monitor = tokio::spawn(async move {
                    // Wait for child process to exit — this blocks until the container dies
                    // Note: child is moved into PoolContainer, so we monitor via docker wait
                    let result = tokio::process::Command::new("docker")
                        .args(["wait", &exit_name])
                        .output()
                        .await;
                    match result {
                        Ok(output) => {
                            let code = String::from_utf8_lossy(&output.stdout).trim().to_string();
                            warn!(container = %exit_name, exit_code = %code, "pool container exited");
                        }
                        Err(e) => {
                            error!(container = %exit_name, err = %e, "docker wait failed");
                        }
                    }
                    // Purge stale IPC files, then clear pool container state
                    exit_queue.purge_stale_ipc(&exit_folder).await;
                    exit_queue.clear_pool_container(&exit_jid).await;
                });

                // Store the warm container
                queue.set_pool_container(chat_jid, pc).await;

                // Process output frames from the delivery channel
                let sessions_clone = sessions.clone();
                let group_folder = group.folder.clone();
                let pool_cb = pool.clone();
                let queue_cb = queue.clone();
                let chat_jid_owned = chat_jid.to_string();

                while let Some(output) = delivery_rx.recv().await {
                    if let Some(ref sid) = output.new_session_id {
                        let mut s = sessions_clone.write().await;
                        s.insert(group_folder.clone(), sid.clone());
                        if let Err(e) = pool_cb.set_session(&group_folder, sid).await {
                            warn!(err = %e, "failed to persist session");
                        }
                    }

                    if let Some(ref result_text) = output.result {
                        let text = strip_internal_blocks(result_text);
                        if !text.is_empty() {
                            let bot_msg = intercom_core::NewMessage {
                                id: format!("bot-{}", chrono::Utc::now().timestamp_millis()),
                                chat_jid: chat_jid_owned.clone(),
                                sender: "bot".into(),
                                sender_name: assistant_name.to_string(),
                                content: text.clone(),
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                is_from_me: true,
                                is_bot_message: true,
                            };
                            if let Err(e) = pool_cb.store_message(&bot_msg).await {
                                warn!(err = %e, "failed to store bot response (pool spawn)");
                            }

                            typing_cancel.notify_one();
                            if let Err(e) = telegram.send_text_to_jid(&chat_jid_owned, &text).await {
                                error!(err = %e, "failed to send agent output via Telegram (pool spawn)");
                            }
                            output_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }

                    // Container signals idle — first message done, container stays warm
                    if output.status == ContainerStatus::Success && output.result.is_none() {
                        queue_cb.notify_idle(&chat_jid_owned).await;
                        break;
                    }
                }

                typing_cancel.notify_one();
                if let Some(handle) = typing_handle {
                    handle.abort();
                }

                return Ok(true);
            }
            Err(e) => {
                warn!(group = group.name.as_str(), err = %e, "pool_spawn failed, falling back to cold container");
                typing_cancel.notify_one();
                if let Some(handle) = typing_handle {
                    handle.abort();
                }
                // Fall through to legacy run_container_agent path
            }
        }
    }

    // 5f. Legacy cold start — send typing indicator and start periodic refresh
    let is_telegram = chat_jid.starts_with("tg:");
    if is_telegram {
        telegram.send_typing(chat_jid).await;
    }
    let typing_cancel = Arc::new(Notify::new());
    let typing_handle: Option<JoinHandle<()>> = if is_telegram {
        let tg = telegram.clone();
        let jid = chat_jid.to_string();
        let cancel = typing_cancel.clone();
        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(4));
            interval.tick().await; // skip immediate first tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        debug!(jid = %jid, "typing refresh tick");
                        tg.send_typing(&jid).await;
                    }
                    _ = cancel.notified() => {
                        info!(jid = %jid, "typing refresh cancelled");
                        break;
                    }
                }
            }
        }))
    } else {
        None
    };

    // 6. Run container and collect output
    let sessions_clone: Arc<RwLock<HashMap<String, String>>> = sessions.clone();
    let group_folder = group.folder.clone();
    let queue_clone: Arc<GroupQueue> = queue.clone();
    let chat_jid_owned = chat_jid.to_string();

    let telegram_cb: Arc<TelegramBridge> = telegram.clone();
    let pool_cb = pool.clone();
    let assistant_name_cb = assistant_name.to_string();
    let typing_cancel_cb = typing_cancel.clone();

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
            let typing_cancel = typing_cancel_cb.clone();

            Box::pin(async move {
                debug!(
                    status = ?output.status,
                    has_result = output.result.is_some(),
                    has_session = output.new_session_id.is_some(),
                    has_event = output.event.is_some(),
                    "on_output callback fired"
                );

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
                        // Persist before deliver: store in Postgres first so output
                        // survives delivery failures or process crashes
                        let bot_msg = intercom_core::NewMessage {
                            id: format!("bot-{}", chrono::Utc::now().timestamp_millis()),
                            chat_jid: chat_jid.clone(),
                            sender: "bot".into(),
                            sender_name: assistant_name.clone(),
                            content: text.clone(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            is_from_me: true,
                            is_bot_message: true,
                        };
                        if let Err(e) = pool.store_message(&bot_msg).await {
                            warn!(err = %e, "failed to store bot response");
                        }

                        // Stop typing refresh before sending — prevents race where
                        // a refresh fires between message delivery and handle abort
                        typing_cancel.notify_one();
                        info!(jid = %chat_jid, "sending agent output, typing cancelled");

                        // Deliver via Telegram
                        if let Err(e) = telegram
                            .send_text_to_jid(&chat_jid, &text)
                            .await
                        {
                            error!(err = %e, "failed to send agent output via Telegram");
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

    // Register the container process in the queue after spawn so IPC features
    // (kill_group, send_message, notify_idle) can find it.
    let on_spawn: Option<Arc<crate::container::runner::OnSpawnCallback>> = {
        let q = queue.clone();
        let jid = chat_jid.to_string();
        let folder = group.folder.clone();
        Some(Arc::new(Box::new(move |container_name: String| {
            let q = q.clone();
            let jid = jid.clone();
            let folder = folder.clone();
            Box::pin(async move {
                q.register_process(&jid, &container_name, Some(folder.as_str())).await;
            })
        })))
    };

    let result = run_container_agent(
        &group_info,
        &input,
        runtime,
        is_main,
        run_config,
        on_output,
        on_spawn,
    )
    .await;

    // Stop typing indicator refresh (belt-and-suspenders with Notify)
    typing_cancel.notify_one();
    if let Some(handle) = typing_handle {
        handle.abort();
    }

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
                // If the error was a result timeout (session bloat), clear
                // the in-memory session so the retry starts fresh.
                let is_result_timeout = run_result
                    .output
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("session auto-reset"));
                if is_result_timeout {
                    warn!(
                        group = group.name.as_str(),
                        "result timeout — clearing session for fresh retry"
                    );
                    {
                        let mut s = sessions.write().await;
                        s.remove(&group.folder);
                    }
                    if let Err(e) = pool.delete_session(&group.folder).await {
                        warn!(err = %e, "failed to delete session from Postgres after result timeout");
                    }
                }

                // Error, but if we already sent output, don't rollback cursor
                if output_sent.load(std::sync::atomic::Ordering::SeqCst) {
                    warn!(
                        group = group.name.as_str(),
                        "agent error after output sent, skipping cursor rollback"
                    );
                    return Ok(true);
                }

                // Rollback cursor for retry
                {
                    let mut ts = shared_timestamps.write().await;
                    ts.0.insert(chat_jid.to_string(), previous_cursor);
                    message_loop::save_agent_timestamps_pub(pool, &ts).await;
                }
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
            {
                let mut ts = shared_timestamps.write().await;
                ts.0.insert(chat_jid.to_string(), previous_cursor);
                message_loop::save_agent_timestamps_pub(pool, &ts).await;
            }
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
