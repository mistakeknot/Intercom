use anyhow::{Context, anyhow};
use intercom_core::IntercomConfig;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::debug;

pub const TELEGRAM_MAX_TEXT_CHARS: usize = 4096;
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

#[derive(Clone)]
pub struct TelegramBridge {
    client: Client,
    bot_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramSendRequest {
    pub jid: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramSendResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub message_ids: Vec<String>,
    pub chunks_planned: usize,
    pub chunks_sent: usize,
    pub chunk_lengths: Vec<usize>,
    pub parity: TelegramSendParity,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramSendParity {
    pub max_chars_per_chunk: usize,
    pub all_chunks_within_limit: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramEditRequest {
    pub jid: String,
    pub message_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramEditResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub truncated: bool,
    pub parity_max_chars: usize,
}

/// Inline keyboard button for Telegram Bot API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardButton {
    pub text: String,
    pub callback_data: String,
}

/// Inline keyboard markup (array of button rows).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

/// Extended send request with optional inline keyboard.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramSendWithButtonsRequest {
    pub jid: String,
    pub text: String,
    #[serde(default)]
    pub reply_markup: Option<InlineKeyboardMarkup>,
}

#[derive(Debug, Deserialize)]
struct TelegramApiEnvelope {
    ok: bool,
    result: Option<serde_json::Value>,
    description: Option<String>,
}

impl TelegramBridge {
    pub fn new(_config: &IntercomConfig) -> Self {
        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        Self {
            client: Client::new(),
            bot_token,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.bot_token.is_some()
    }

    /// Send "typing..." indicator to a Telegram chat.
    /// Fire-and-forget: errors are logged but don't propagate.
    pub async fn send_typing(&self, jid: &str) {
        let Some(ref token) = self.bot_token else {
            return;
        };
        let chat_id = normalize_chat_id(jid);
        let endpoint = format!("{TELEGRAM_API_BASE}/bot{token}/sendChatAction");
        if let Err(e) = self
            .client
            .post(&endpoint)
            .json(&serde_json::json!({"chat_id": chat_id, "action": "typing"}))
            .send()
            .await
        {
            debug!(jid, err = %e, "sendChatAction failed");
        }
    }

    /// Convenience: send a text message to a JID (chat_id).
    /// Used by the orchestrator to deliver agent output.
    pub async fn send_text_to_jid(&self, jid: &str, text: &str) -> anyhow::Result<()> {
        self.send_message(TelegramSendRequest {
            jid: jid.to_string(),
            text: text.to_string(),
        })
        .await?;
        Ok(())
    }

    pub async fn send_message(
        &self,
        request: TelegramSendRequest,
    ) -> anyhow::Result<TelegramSendResponse> {
        let token = self
            .bot_token
            .as_ref()
            .ok_or_else(|| anyhow!("TELEGRAM_BOT_TOKEN is not set for intercomd"))?;

        if request.text.trim().is_empty() {
            return Err(anyhow!("cannot send an empty Telegram message"));
        }

        let chat_id = normalize_chat_id(&request.jid);
        let endpoint = format!("{TELEGRAM_API_BASE}/bot{token}/sendMessage");
        let chunks = split_for_telegram(&request.text, TELEGRAM_MAX_TEXT_CHARS);
        let chunk_lengths = chunks
            .iter()
            .map(|chunk| chunk.chars().count())
            .collect::<Vec<_>>();
        let mut sent_calls = 0_usize;
        let mut message_ids = Vec::new();

        for chunk in &chunks {
            let response = self
                .client
                .post(&endpoint)
                .json(&serde_json::json!({
                    "chat_id": chat_id,
                    "text": chunk,
                }))
                .send()
                .await
                .context("failed to call Telegram sendMessage")?;

            let body: TelegramApiEnvelope = response
                .json()
                .await
                .context("failed to parse Telegram sendMessage response")?;
            if !body.ok {
                return Err(anyhow!(body.description.unwrap_or_else(|| {
                    "Telegram sendMessage returned ok=false".to_string()
                })));
            }

            sent_calls += 1;
            if let Some(message_id) = body
                .result
                .as_ref()
                .and_then(|value| value.get("message_id"))
                .and_then(|value| value.as_i64())
            {
                message_ids.push(message_id.to_string());
            }
        }

        Ok(TelegramSendResponse {
            ok: true,
            error: None,
            message_ids,
            chunks_planned: chunks.len(),
            chunks_sent: sent_calls,
            chunk_lengths: chunk_lengths.clone(),
            parity: TelegramSendParity {
                max_chars_per_chunk: TELEGRAM_MAX_TEXT_CHARS,
                all_chunks_within_limit: chunk_lengths
                    .iter()
                    .all(|len| *len <= TELEGRAM_MAX_TEXT_CHARS),
            },
        })
    }

    pub async fn edit_message(
        &self,
        request: TelegramEditRequest,
    ) -> anyhow::Result<TelegramEditResponse> {
        let token = self
            .bot_token
            .as_ref()
            .ok_or_else(|| anyhow!("TELEGRAM_BOT_TOKEN is not set for intercomd"))?;
        let chat_id = normalize_chat_id(&request.jid);
        let message_id = request
            .message_id
            .parse::<i64>()
            .with_context(|| format!("invalid message_id `{}`", request.message_id))?;

        let (text, truncated) = truncate_for_telegram(&request.text, TELEGRAM_MAX_TEXT_CHARS);
        let endpoint = format!("{TELEGRAM_API_BASE}/bot{token}/editMessageText");
        let response = self
            .client
            .post(&endpoint)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "message_id": message_id,
                "text": text,
            }))
            .send()
            .await
            .context("failed to call Telegram editMessageText")?;

        let body: TelegramApiEnvelope = response
            .json()
            .await
            .context("failed to parse Telegram editMessageText response")?;
        if !body.ok {
            return Err(anyhow!(body.description.unwrap_or_else(|| {
                "Telegram editMessageText returned ok=false".to_string()
            })));
        }

        Ok(TelegramEditResponse {
            ok: true,
            error: None,
            truncated,
            parity_max_chars: TELEGRAM_MAX_TEXT_CHARS,
        })
    }

    /// Send a message with optional inline keyboard buttons.
    /// Falls back to plain send_message if reply_markup is None.
    pub async fn send_message_with_buttons(
        &self,
        request: TelegramSendWithButtonsRequest,
    ) -> anyhow::Result<TelegramSendResponse> {
        if request.reply_markup.is_none() {
            return self
                .send_message(TelegramSendRequest {
                    jid: request.jid,
                    text: request.text,
                })
                .await;
        }

        let token = self
            .bot_token
            .as_ref()
            .ok_or_else(|| anyhow!("TELEGRAM_BOT_TOKEN is not set for intercomd"))?;

        let chat_id = normalize_chat_id(&request.jid);
        let endpoint = format!("{TELEGRAM_API_BASE}/bot{token}/sendMessage");

        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": &request.text,
        });
        if let Some(markup) = &request.reply_markup {
            body["reply_markup"] = serde_json::to_value(markup)
                .context("failed to serialize InlineKeyboardMarkup")?;
        }

        let response = self
            .client
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .context("failed to call Telegram sendMessage")?;

        let envelope: TelegramApiEnvelope = response
            .json()
            .await
            .context("failed to parse Telegram sendMessage response")?;
        if !envelope.ok {
            return Err(anyhow!(envelope.description.unwrap_or_else(|| {
                "Telegram sendMessage returned ok=false".to_string()
            })));
        }

        let message_id = envelope
            .result
            .as_ref()
            .and_then(|v| v.get("message_id"))
            .and_then(|v| v.as_i64())
            .map(|id| id.to_string())
            .unwrap_or_default();

        Ok(TelegramSendResponse {
            ok: true,
            error: None,
            message_ids: vec![message_id],
            chunks_planned: 1,
            chunks_sent: 1,
            chunk_lengths: vec![request.text.chars().count()],
            parity: TelegramSendParity {
                max_chars_per_chunk: TELEGRAM_MAX_TEXT_CHARS,
                all_chunks_within_limit: request.text.chars().count() <= TELEGRAM_MAX_TEXT_CHARS,
            },
        })
    }

    /// Answer a Telegram callback query (acknowledge button press).
    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
    ) -> anyhow::Result<()> {
        let token = self
            .bot_token
            .as_ref()
            .ok_or_else(|| anyhow!("TELEGRAM_BOT_TOKEN is not set for intercomd"))?;

        let endpoint = format!("{TELEGRAM_API_BASE}/bot{token}/answerCallbackQuery");
        let mut body = serde_json::json!({
            "callback_query_id": callback_query_id,
        });
        if let Some(t) = text {
            body["text"] = serde_json::json!(t);
        }

        self.client
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .context("failed to call Telegram answerCallbackQuery")?;

        Ok(())
    }

}

impl TelegramSendResponse {
    pub fn from_error(err: impl Into<String>) -> Self {
        let error = err.into();
        Self {
            ok: false,
            error: Some(error),
            message_ids: Vec::new(),
            chunks_planned: 0,
            chunks_sent: 0,
            chunk_lengths: Vec::new(),
            parity: TelegramSendParity {
                max_chars_per_chunk: TELEGRAM_MAX_TEXT_CHARS,
                all_chunks_within_limit: true,
            },
        }
    }
}

impl TelegramEditResponse {
    pub fn from_error(err: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(err.into()),
            truncated: false,
            parity_max_chars: TELEGRAM_MAX_TEXT_CHARS,
        }
    }
}

fn normalize_chat_id(jid: &str) -> &str {
    jid.strip_prefix("tg:").unwrap_or(jid)
}

fn split_for_telegram(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut chars_in_current = 0_usize;

    for ch in text.chars() {
        if chars_in_current >= max_chars {
            chunks.push(current);
            current = String::new();
            chars_in_current = 0;
        }
        current.push(ch);
        chars_in_current += 1;
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

fn truncate_for_telegram(text: &str, max_chars: usize) -> (String, bool) {
    let mut output = String::new();
    let mut count = 0_usize;

    for ch in text.chars() {
        if count >= max_chars {
            return (output, true);
        }
        output.push(ch);
        count += 1;
    }

    (output, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_for_telegram_keeps_chunks_within_limit() {
        let text = "a".repeat(9005);
        let chunks = split_for_telegram(&text, TELEGRAM_MAX_TEXT_CHARS);
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= TELEGRAM_MAX_TEXT_CHARS)
        );
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.chars().count())
                .sum::<usize>(),
            text.chars().count()
        );
    }

    #[test]
    fn parses_approve_callback_data() {
        let data = "approve:gate-review";
        let parts: Vec<&str> = data.splitn(2, ':').collect();
        assert_eq!(parts[0], "approve");
        assert_eq!(parts[1], "gate-review");
    }

    #[test]
    fn parses_callback_with_colons_in_id() {
        let data = "approve:gate:with:colons";
        let parts: Vec<&str> = data.splitn(2, ':').collect();
        assert_eq!(parts[0], "approve");
        assert_eq!(parts[1], "gate:with:colons");
    }

    #[test]
    fn rejects_invalid_callback_data() {
        let data = "nocolon";
        let parts: Vec<&str> = data.splitn(2, ':').collect();
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn inline_keyboard_serializes_correctly() {
        let markup = InlineKeyboardMarkup {
            inline_keyboard: vec![vec![InlineKeyboardButton {
                text: "OK".to_string(),
                callback_data: "ok:1".to_string(),
            }]],
        };
        let json = serde_json::to_value(&markup).unwrap();
        assert_eq!(json["inline_keyboard"][0][0]["text"].as_str(), Some("OK"));
        assert_eq!(
            json["inline_keyboard"][0][0]["callback_data"].as_str(),
            Some("ok:1")
        );
    }
}
