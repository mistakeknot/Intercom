//! Telegram Bot API long-polling update consumer.
//!
//! Replaces the Node/grammY update poller. Polls `getUpdates`, parses
//! messages (text, photo, document, etc.), slash commands, and callback
//! queries, then routes them into the Rust orchestrator directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use intercom_core::{DemarchAdapter, PgPool, RegisteredGroup};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::{RwLock, watch};
use tracing::{debug, error, info, warn};

use crate::commands;
use crate::queue::GroupQueue;
use crate::telegram::TelegramBridge;

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const DEFAULT_POLL_TIMEOUT: u64 = 30;

// ---------------------------------------------------------------------------
// Telegram Bot API types (subset we care about)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
    callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct Message {
    message_id: i64,
    date: i64,
    chat: Chat,
    from: Option<User>,
    text: Option<String>,
    caption: Option<String>,
    entities: Option<Vec<MessageEntity>>,
    photo: Option<Vec<PhotoSize>>,
    document: Option<Document>,
    video: Option<serde_json::Value>,
    voice: Option<serde_json::Value>,
    audio: Option<serde_json::Value>,
    sticker: Option<Sticker>,
    location: Option<serde_json::Value>,
    contact: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
    title: Option<String>,
    first_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct User {
    id: i64,
    first_name: Option<String>,
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageEntity {
    #[serde(rename = "type")]
    entity_type: String,
    offset: usize,
    length: usize,
}

#[derive(Debug, Deserialize)]
struct PhotoSize {
    file_id: String,
    #[allow(dead_code)]
    width: i32,
    #[allow(dead_code)]
    height: i32,
    #[allow(dead_code)]
    file_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Document {
    file_id: String,
    file_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Sticker {
    emoji: Option<String>,
    #[allow(dead_code)]
    file_id: String,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    id: String,
    from: User,
    message: Option<Message>,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct File {
    file_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BotInfo {
    id: i64,
    username: Option<String>,
}

// ---------------------------------------------------------------------------
// Poller config and state
// ---------------------------------------------------------------------------

pub struct TelegramPollerConfig {
    pub poll_timeout_secs: u64,
    pub groups_dir: PathBuf,
    pub assistant_name: String,
    pub main_group_folder: String,
    pub started_at: Instant,
}

impl Default for TelegramPollerConfig {
    fn default() -> Self {
        Self {
            poll_timeout_secs: DEFAULT_POLL_TIMEOUT,
            groups_dir: PathBuf::from("groups"),
            assistant_name: "Amtiskaw".into(),
            main_group_folder: "main".into(),
            started_at: Instant::now(),
        }
    }
}

/// Long-running Telegram update poller.
pub struct TelegramPoller {
    config: TelegramPollerConfig,
    client: Client,
    bot_token: String,
    telegram: Arc<TelegramBridge>,
    demarch: Arc<DemarchAdapter>,
    pool: PgPool,
    queue: Arc<GroupQueue>,
    groups: Arc<RwLock<HashMap<String, RegisteredGroup>>>,
    sessions: Arc<RwLock<HashMap<String, String>>>,
    bot_username: RwLock<Option<String>>,
    intercom_config: Arc<intercom_core::IntercomConfig>,
}

impl TelegramPoller {
    pub fn new(
        config: TelegramPollerConfig,
        bot_token: String,
        telegram: Arc<TelegramBridge>,
        demarch: Arc<DemarchAdapter>,
        intercom_config: Arc<intercom_core::IntercomConfig>,
        pool: PgPool,
        queue: Arc<GroupQueue>,
        groups: Arc<RwLock<HashMap<String, RegisteredGroup>>>,
        sessions: Arc<RwLock<HashMap<String, String>>>,
    ) -> Self {
        let poll_timeout = config.poll_timeout_secs;
        Self {
            config,
            client: Client::builder()
                .timeout(Duration::from_secs(poll_timeout + 10))
                .build()
                .expect("build reqwest client"),
            bot_token,
            telegram,
            demarch,
            intercom_config,
            pool,
            queue,
            groups,
            sessions,
            bot_username: RwLock::new(None),
        }
    }

    /// Run the polling loop until shutdown signal.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        // Fetch bot info and register commands
        if let Err(e) = self.init_bot().await {
            error!(err = %e, "failed to initialize Telegram bot — poller will not start");
            return;
        }

        let mut offset: i64 = 0;
        info!("Telegram poller started");

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Telegram poller shutting down");
                        return;
                    }
                }
                result = self.get_updates(offset) => {
                    match result {
                        Ok(updates) => {
                            for update in updates {
                                offset = update.update_id + 1;
                                if let Err(e) = self.handle_update(update).await {
                                    warn!(err = %e, "error handling Telegram update");
                                }
                            }
                        }
                        Err(e) => {
                            warn!(err = %e, "getUpdates failed, retrying in 5s");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
            }
        }
    }

    async fn init_bot(&self) -> anyhow::Result<()> {
        // Get bot info
        let url = format!("{TELEGRAM_API_BASE}/bot{}/getMe", self.bot_token);
        let resp: ApiResponse<BotInfo> = self.client.get(&url).send().await?.json().await?;
        let bot = resp.result.ok_or_else(|| anyhow!("getMe returned no result"))?;
        let username = bot.username.clone().unwrap_or_default();
        info!(bot_id = bot.id, bot_username = %username, "Telegram bot connected");
        *self.bot_username.write().await = bot.username;

        // Register command menu
        let url = format!("{TELEGRAM_API_BASE}/bot{}/setMyCommands", self.bot_token);
        let commands = serde_json::json!({
            "commands": [
                {"command": "help", "description": "Show available commands"},
                {"command": "model", "description": "Show or switch model"},
                {"command": "reset", "description": "Clear session and stop container"},
                {"command": "status", "description": "Show runtime, session, and container status"},
                {"command": "ping", "description": "Check if bot is online"},
                {"command": "chatid", "description": "Show this chat's registration ID"},
            ]
        });
        self.client.post(&url).json(&commands).send().await?;

        Ok(())
    }

    async fn get_updates(&self, offset: i64) -> anyhow::Result<Vec<Update>> {
        let url = format!("{TELEGRAM_API_BASE}/bot{}/getUpdates", self.bot_token);
        let resp: ApiResponse<Vec<Update>> = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "offset": offset,
                "timeout": self.config.poll_timeout_secs,
                "allowed_updates": ["message", "callback_query"],
            }))
            .send()
            .await
            .context("getUpdates request failed")?
            .json()
            .await
            .context("getUpdates parse failed")?;

        if !resp.ok {
            return Err(anyhow!(
                "getUpdates error: {}",
                resp.description.unwrap_or_default()
            ));
        }

        Ok(resp.result.unwrap_or_default())
    }

    async fn handle_update(&self, update: Update) -> anyhow::Result<()> {
        if let Some(cq) = update.callback_query {
            return self.handle_callback_query(cq).await;
        }

        if let Some(msg) = update.message {
            return self.handle_message(msg).await;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Message handling
    // -----------------------------------------------------------------------

    async fn handle_message(&self, msg: Message) -> anyhow::Result<()> {
        let chat_jid = format!("tg:{}", msg.chat.id);
        let timestamp =
            chrono::DateTime::from_timestamp(msg.date, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
        let sender_name = msg
            .from
            .as_ref()
            .and_then(|u| u.first_name.as_deref())
            .or_else(|| msg.from.as_ref().and_then(|u| u.username.as_deref()))
            .unwrap_or("Unknown")
            .to_string();
        let sender_id = msg
            .from
            .as_ref()
            .map(|u| u.id.to_string())
            .unwrap_or_default();
        let msg_id = msg.message_id.to_string();
        let chat_name = match msg.chat.chat_type.as_str() {
            "private" => sender_name.clone(),
            _ => msg.chat.title.clone().unwrap_or_else(|| chat_jid.clone()),
        };
        let is_group = msg.chat.chat_type != "private";

        // Check for slash commands first
        if let Some(ref text) = msg.text {
            if text.starts_with('/') {
                return self
                    .handle_command(&chat_jid, text, &chat_name, is_group, &msg)
                    .await;
            }
        }

        // Determine content: text, photo, document, or placeholder
        let content = self
            .resolve_content(&msg, &chat_jid)
            .await;

        let content = match content {
            Some(c) if !c.is_empty() => c,
            _ => return Ok(()), // no meaningful content
        };

        // Store chat metadata in Postgres
        self.store_chat_metadata(&chat_jid, &timestamp, &chat_name, is_group)
            .await;

        // Look up registered group
        let group = {
            let groups = self.groups.read().await;
            groups.get(&chat_jid).cloned()
        };

        let group = match group {
            Some(g) => g,
            None => {
                debug!(chat_jid = %chat_jid, "message from unregistered chat");
                return Ok(());
            }
        };

        // Check trigger requirement
        let is_main = group.folder == self.config.main_group_folder;
        let requires_trigger = !is_main && group.requires_trigger.unwrap_or(true);

        let content = if requires_trigger {
            let trigger = &group.trigger;
            if !trigger_matches(&content, trigger) {
                // Check for @bot_username mention as alternative trigger
                let content_with_trigger =
                    self.translate_bot_mention(&content, &msg).await;
                if let Some(c) = content_with_trigger {
                    c
                } else {
                    debug!(chat_jid = %chat_jid, "trigger required but not present");
                    return Ok(());
                }
            } else {
                content
            }
        } else {
            // Still translate @bot_username mentions for non-trigger groups
            self.translate_bot_mention(&content, &msg)
                .await
                .unwrap_or(content)
        };

        // Store message in Postgres
        let pg_msg = intercom_core::NewMessage {
            id: msg_id,
            chat_jid: chat_jid.clone(),
            sender: sender_id,
            sender_name,
            content,
            timestamp,
            is_from_me: false,
            is_bot_message: false,
        };
        if let Err(e) = self.pool.store_message(&pg_msg).await {
            warn!(err = %e, chat_jid = %chat_jid, "failed to store inbound message");
        }

        // Enqueue for processing
        self.queue.enqueue_message_check(&chat_jid).await;

        Ok(())
    }

    /// Resolve message content: text, photo path, document path, or placeholder.
    async fn resolve_content(&self, msg: &Message, chat_jid: &str) -> Option<String> {
        let caption = msg.caption.as_deref().map(|c| format!(" {c}")).unwrap_or_default();

        // Text messages
        if let Some(ref text) = msg.text {
            return Some(text.clone());
        }

        // Photo — download largest size
        if let Some(ref photos) = msg.photo {
            if let Some(largest) = photos.last() {
                let group_folder = self.get_group_folder(chat_jid).await;
                if let Some(folder) = group_folder {
                    if let Some(rel) = self.download_file(&largest.file_id, &folder, ".jpg").await {
                        return Some(format!("[Image: {rel}]{caption}"));
                    }
                }
                return Some(format!("[Photo]{caption}"));
            }
        }

        // Document
        if let Some(ref doc) = msg.document {
            let group_folder = self.get_group_folder(chat_jid).await;
            let orig_name = doc.file_name.as_deref().unwrap_or("file");
            let ext = Path::new(orig_name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            if let Some(folder) = group_folder {
                if let Some(rel) = self.download_file(&doc.file_id, &folder, &ext).await {
                    return Some(format!("[Document: {rel}]{caption}"));
                }
            }
            return Some(format!("[Document: {orig_name}]{caption}"));
        }

        // Placeholders for other media types
        if msg.video.is_some() {
            return Some(format!("[Video]{caption}"));
        }
        if msg.voice.is_some() {
            return Some(format!("[Voice message]{caption}"));
        }
        if msg.audio.is_some() {
            return Some(format!("[Audio]{caption}"));
        }
        if let Some(ref sticker) = msg.sticker {
            let emoji = sticker.emoji.as_deref().unwrap_or("");
            return Some(format!("[Sticker {emoji}]"));
        }
        if msg.location.is_some() {
            return Some("[Location]".to_string());
        }
        if msg.contact.is_some() {
            return Some("[Contact]".to_string());
        }

        None
    }

    /// Download a Telegram file to the group's media directory.
    /// Returns the relative path (e.g., `media/123_1709600000.jpg`).
    async fn download_file(
        &self,
        file_id: &str,
        group_folder: &str,
        ext: &str,
    ) -> Option<String> {
        // Step 1: getFile to get file_path
        let url = format!("{TELEGRAM_API_BASE}/bot{}/getFile", self.bot_token);
        let resp: ApiResponse<File> = self
            .client
            .post(&url)
            .json(&serde_json::json!({"file_id": file_id}))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;

        let file_path = resp.result?.file_path?;

        // Step 2: Download the file
        let download_url = format!(
            "{TELEGRAM_API_BASE}/file/bot{}/{}",
            self.bot_token, file_path
        );
        let bytes = self
            .client
            .get(&download_url)
            .send()
            .await
            .ok()?
            .bytes()
            .await
            .ok()?;

        // Step 3: Save to disk
        let media_dir = self.config.groups_dir.join(group_folder).join("media");
        tokio::fs::create_dir_all(&media_dir).await.ok()?;

        let timestamp = chrono::Utc::now().timestamp_millis();
        let filename = format!("{file_id}_{timestamp}{ext}");
        let dest = media_dir.join(&filename);
        tokio::fs::write(&dest, &bytes).await.ok()?;

        Some(format!("media/{filename}"))
    }

    async fn get_group_folder(&self, chat_jid: &str) -> Option<String> {
        let groups = self.groups.read().await;
        groups.get(chat_jid).map(|g| g.folder.clone())
    }

    /// Translate @bot_username mentions into the trigger pattern.
    async fn translate_bot_mention(&self, content: &str, msg: &Message) -> Option<String> {
        let bot_username = self.bot_username.read().await;
        let bot_username = bot_username.as_deref()?;
        let bot_username_lower = bot_username.to_lowercase();

        let entities = msg.entities.as_ref()?;
        let is_mentioned = entities.iter().any(|e| {
            if e.entity_type == "mention" {
                let mention: String = content.chars().skip(e.offset).take(e.length).collect();
                mention.to_lowercase() == format!("@{bot_username_lower}")
            } else {
                false
            }
        });

        if is_mentioned {
            Some(format!("@{} {}", self.config.assistant_name, content))
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // Command handling
    // -----------------------------------------------------------------------

    async fn handle_command(
        &self,
        chat_jid: &str,
        text: &str,
        chat_name: &str,
        is_group: bool,
        msg: &Message,
    ) -> anyhow::Result<()> {
        // Parse "/command args" or "/command@botname args"
        let text = text.trim();
        let (cmd, args) = match text.split_once(' ') {
            Some((c, a)) => (c, a.trim()),
            None => (text, ""),
        };
        // Strip leading / and optional @botname suffix
        let cmd = cmd.strip_prefix('/').unwrap_or(cmd);
        let cmd = cmd.split('@').next().unwrap_or(cmd);

        let timestamp = chrono::DateTime::from_timestamp(msg.date, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();

        // Store chat metadata
        self.store_chat_metadata(chat_jid, &timestamp, chat_name, is_group)
            .await;

        match cmd {
            "chatid" => {
                let chat_type = &msg.chat.chat_type;
                let reply = format!(
                    "Chat ID: `{chat_jid}`\nName: {chat_name}\nType: {chat_type}"
                );
                self.telegram
                    .send_text_to_jid(chat_jid, &reply)
                    .await?;
            }
            "ping" => {
                let reply = format!("{} is online.", self.config.assistant_name);
                self.telegram.send_text_to_jid(chat_jid, &reply).await?;
            }
            "help" | "model" | "reset" | "new" | "status" => {
                // Look up group state for the command handler
                let (group_name, group_folder, current_model, session_id, container_active) = {
                    let groups = self.groups.read().await;
                    let sessions = self.sessions.read().await;
                    match groups.get(chat_jid) {
                        Some(g) => {
                            let sid = sessions.get(&g.folder).cloned();
                            let active = self.queue.is_active(chat_jid).await;
                            (
                                Some(g.name.clone()),
                                Some(g.folder.clone()),
                                g.model.clone(),
                                sid,
                                active,
                            )
                        }
                        None => (None, None, None, None, false),
                    }
                };

                // Fetch run info for /status enrichment
                let run_info = if cmd == "status" {
                    crate::fetch_run_info(&self.demarch).await
                } else {
                    None
                };

                let ctx = commands::CommandContext {
                    assistant_name: self.config.assistant_name.clone(),
                    started_at: self.config.started_at,
                };

                let result = commands::handle_command(
                    cmd,
                    args,
                    group_name.as_deref(),
                    group_folder.as_deref(),
                    current_model.as_deref(),
                    session_id.as_deref(),
                    container_active,
                    &ctx,
                    run_info.as_ref(),
                );

                // Apply side effects
                self.apply_effects(chat_jid, group_folder.as_deref(), &result.effects)
                    .await;

                // Send response
                self.telegram
                    .send_text_to_jid(chat_jid, &result.text)
                    .await?;
            }
            _ => {
                // Unknown command — ignore
            }
        }

        Ok(())
    }

    async fn apply_effects(
        &self,
        chat_jid: &str,
        group_folder: Option<&str>,
        effects: &[commands::CommandEffect],
    ) {
        for effect in effects {
            match effect {
                commands::CommandEffect::KillContainer => {
                    self.queue.kill_group(chat_jid).await;
                }
                commands::CommandEffect::ClearSession => {
                    if let Some(folder) = group_folder {
                        self.sessions.write().await.remove(folder);
                        if let Err(e) = self.pool.delete_session(folder).await {
                            warn!(err = %e, folder, "failed to delete session");
                        }
                    }
                }
                commands::CommandEffect::SwitchModel {
                    model_id,
                    runtime,
                } => {
                    if let Some(folder) = group_folder {
                        let mut groups = self.groups.write().await;
                        if let Some(group) =
                            groups.values_mut().find(|g| g.folder == folder)
                        {
                            group.model = Some(model_id.clone());
                            group.runtime = Some(runtime.clone());
                            if let Err(e) = self.pool.set_registered_group(group).await {
                                warn!(err = %e, folder, "failed to persist model switch");
                            }
                        }
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Callback query handling
    // -----------------------------------------------------------------------

    async fn handle_callback_query(&self, cq: CallbackQuery) -> anyhow::Result<()> {
        let data = match cq.data {
            Some(d) => d,
            None => return Ok(()),
        };

        let chat_id = cq
            .message
            .as_ref()
            .map(|m| m.chat.id)
            .ok_or_else(|| anyhow!("callback query without chat"))?;
        let chat_jid = format!("tg:{chat_id}");
        let message_id = cq
            .message
            .as_ref()
            .map(|m| m.message_id.to_string())
            .unwrap_or_default();

        // Authorization: verify chat is registered using Postgres groups (not SQLite)
        let group = {
            let groups = self.groups.read().await;
            groups.get(&chat_jid).cloned()
        };
        let group = match group {
            Some(g) => g,
            None => {
                self.telegram
                    .answer_callback_query(&cq.id, Some("Unauthorized: unregistered chat"))
                    .await?;
                return Ok(());
            }
        };
        let is_main = group.folder == self.config.main_group_folder;

        // Parse action:target_id
        let parts: Vec<&str> = data.splitn(2, ':').collect();
        if parts.len() != 2 {
            self.telegram
                .answer_callback_query(&cq.id, Some("Invalid action"))
                .await?;
            return Ok(());
        }

        let action = parts[0];
        let target_id = parts[1].to_string();
        let sender = cq.from.first_name.as_deref()
            .or(cq.from.username.as_deref())
            .unwrap_or("unknown");

        // Execute the write operation via demarch
        let (write_result, status_text) = match action {
            "approve" => {
                let resp = self.demarch.execute_write(
                    intercom_core::WriteOperation::ApproveGate {
                        gate_id: Some(target_id.clone()),
                        reason: Some(format!("Approved by {sender} via Telegram")),
                    },
                    is_main,
                );
                let ok = resp.status == intercom_core::DemarchStatus::Ok;
                let status = if ok {
                    format!("✅ Gate {target_id} approved by @{sender}")
                } else {
                    format!("❌ Failed: {}", resp.result)
                };
                (resp.result, status)
            }
            "reject" => {
                let resp = self.demarch.execute_write(
                    intercom_core::WriteOperation::RejectGate {
                        gate_id: Some(target_id.clone()),
                        reason: Some(format!("Rejected by {sender} via Telegram")),
                    },
                    is_main,
                );
                let ok = resp.status == intercom_core::DemarchStatus::Ok;
                let status = if ok {
                    format!("❌ Gate {target_id} rejected by @{sender}")
                } else {
                    format!("❌ Failed: {}", resp.result)
                };
                (resp.result, status)
            }
            "defer" => {
                let resp = self.demarch.execute_write(
                    intercom_core::WriteOperation::DeferGate {
                        gate_id: Some(target_id.clone()),
                        reason: Some(format!("Deferred by {sender} via Telegram")),
                    },
                    is_main,
                );
                let ok = resp.status == intercom_core::DemarchStatus::Ok;
                let status = if ok {
                    format!("⏸️ Gate {target_id} deferred by @{sender}")
                } else {
                    format!("❌ Failed: {}", resp.result)
                };
                (resp.result, status)
            }
            "extend" => {
                let resp = self.demarch.execute_write(
                    intercom_core::WriteOperation::ExtendBudget {
                        run_id: target_id.clone(),
                        max_dispatches: None,
                    },
                    is_main,
                );
                let ok = resp.status == intercom_core::DemarchStatus::Ok;
                let status = if ok {
                    format!("💰 Budget extended for run {target_id} by @{sender}")
                } else {
                    format!("❌ Failed: {}", resp.result)
                };
                (resp.result, status)
            }
            "cancel" => {
                let resp = self.demarch.execute_write(
                    intercom_core::WriteOperation::CancelRun {
                        run_id: target_id.clone(),
                        reason: Some(format!("Cancelled by {sender} via Telegram")),
                    },
                    is_main,
                );
                let ok = resp.status == intercom_core::DemarchStatus::Ok;
                let status = if ok {
                    format!("🛑 Run {target_id} cancelled by @{sender}")
                } else {
                    format!("❌ Failed: {}", resp.result)
                };
                (resp.result, status)
            }
            _ => {
                self.telegram
                    .answer_callback_query(&cq.id, Some("Unknown action"))
                    .await?;
                return Ok(());
            }
        };

        let _ = write_result;

        // Edit the original message to show result (removes buttons)
        let _ = self
            .telegram
            .edit_message(crate::telegram::TelegramEditRequest {
                jid: chat_jid,
                message_id,
                text: status_text.clone(),
            })
            .await;

        // Answer the callback query (dismisses loading spinner)
        self.telegram
            .answer_callback_query(&cq.id, Some(&status_text))
            .await?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    async fn store_chat_metadata(
        &self,
        chat_jid: &str,
        timestamp: &str,
        chat_name: &str,
        is_group: bool,
    ) {
        if let Err(e) = self
            .pool
            .store_chat_metadata(chat_jid, timestamp, Some(chat_name), Some("telegram"), Some(is_group))
            .await
        {
            debug!(err = %e, "failed to store chat metadata");
        }
    }
}

fn trigger_matches(content: &str, trigger: &str) -> bool {
    let trigger = trigger.trim();
    if trigger.is_empty() {
        return true;
    }
    let content = content.trim_start();
    if content.len() < trigger.len() {
        return false;
    }
    content
        .get(..trigger.len())
        .map(|prefix| prefix.eq_ignore_ascii_case(trigger))
        .unwrap_or(false)
}
