//! Event consumer loop — polls `ic events tail --consumer=intercom` and
//! routes relevant kernel events to the Telegram bridge as push notifications.
//!
//! Event types handled:
//! - `gate.pending`    → send approval request with inline buttons
//! - `run.completed`   → send completion notice
//! - `budget.exceeded` → send budget alert
//! - `phase.changed`   → send phase transition notice

use std::sync::Arc;
use std::time::Duration;

use intercom_core::{DemarchAdapter, ReadOperation};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::ipc::IpcDelegate;
use crate::telegram::{InlineKeyboardButton, InlineKeyboardMarkup};

/// Configuration for the event consumer loop.
#[derive(Debug, Clone)]
pub struct EventConsumerConfig {
    /// Poll interval for `ic events tail`.
    pub poll_interval: Duration,
    /// Maximum events per poll.
    pub batch_size: u32,
    /// Chat JID to send notifications to (main group).
    pub notification_jid: Option<String>,
    /// Enable/disable the event consumer.
    pub enabled: bool,
}

impl Default for EventConsumerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            batch_size: 20,
            notification_jid: None,
            enabled: false,
        }
    }
}

/// A kernel event from `ic events tail`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelEvent {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    pub kind: Option<String>,
    pub run_id: Option<String>,
    pub phase: Option<String>,
    pub gate_id: Option<String>,
    pub reason: Option<String>,
    pub timestamp: Option<String>,
    /// Catch-all for fields we don't model explicitly.
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

/// A formatted notification with optional inline keyboard buttons.
struct Notification {
    text: String,
    buttons: Option<InlineKeyboardMarkup>,
}

/// Build inline keyboard for gate approval.
/// TODO(iv-followup): Add Reject/Defer buttons once WriteOperation variants exist.
fn gate_approval_buttons(gate_id: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup {
        inline_keyboard: vec![vec![InlineKeyboardButton {
            text: "✅ Approve".to_string(),
            callback_data: format!("approve:{gate_id}"),
        }]],
    }
}

// TODO(iv-followup): Add budget_action_buttons once ExtendBudget/CancelRun
// WriteOperation variants exist. Budget notifications are text-only for now.

/// The event consumer. Polls for kernel events and sends notifications.
pub struct EventConsumer {
    config: EventConsumerConfig,
    demarch: Arc<DemarchAdapter>,
    delegate: Arc<dyn IpcDelegate>,
    /// Last seen event ID — used as `since` cursor for next poll.
    last_event_id: Option<String>,
}

impl EventConsumer {
    pub fn new(
        config: EventConsumerConfig,
        demarch: Arc<DemarchAdapter>,
        delegate: Arc<dyn IpcDelegate>,
    ) -> Self {
        Self {
            config,
            demarch,
            delegate,
            last_event_id: None,
        }
    }

    /// Run the event consumer loop. Call from a tokio::spawn.
    pub async fn run(&mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        if !self.config.enabled {
            info!("Event consumer disabled — skipping");
            return;
        }

        let jid = match &self.config.notification_jid {
            Some(jid) if !jid.is_empty() => jid.clone(),
            _ => {
                warn!("Event consumer enabled but no notification_jid configured — skipping");
                return;
            }
        };

        info!(
            jid = %jid,
            poll_interval_ms = %self.config.poll_interval.as_millis(),
            "Event consumer started"
        );

        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.config.poll_interval) => {
                    self.poll_events(&jid);
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Event consumer shutting down");
                        return;
                    }
                }
            }
        }
    }

    /// Poll for new events and dispatch notifications.
    fn poll_events(&mut self, notification_jid: &str) {
        let response = self.demarch.execute_read(ReadOperation::RunEvents {
            limit: Some(self.config.batch_size),
            since: self.last_event_id.clone(),
        });

        if response.status != intercom_core::DemarchStatus::Ok {
            debug!(
                result = %response.result,
                "Event poll returned non-ok (kernel may be unavailable)"
            );
            return;
        }

        let events: Vec<KernelEvent> = match serde_json::from_str(&response.result) {
            Ok(events) => events,
            Err(err) => {
                // Might be a single object or empty string
                debug!(err = %err, "Failed to parse events response as array");
                return;
            }
        };

        if events.is_empty() {
            return;
        }

        debug!(count = events.len(), "Processing kernel events");

        for event in &events {
            if let Some(notif) = self.format_notification(event) {
                if notif.buttons.is_some() {
                    self.delegate.send_message_with_buttons(
                        notification_jid,
                        &notif.text,
                        Some("Intercom"),
                        notif.buttons,
                    );
                } else {
                    self.delegate
                        .send_message(notification_jid, &notif.text, Some("Intercom"));
                }
            }

            // Advance cursor
            if let Some(id) = &event.id {
                self.last_event_id = Some(id.clone());
            }
        }
    }

    /// Format a kernel event into a notification with optional inline buttons.
    /// Returns None for events we don't care about.
    fn format_notification(&self, event: &KernelEvent) -> Option<Notification> {
        let kind = event
            .kind
            .as_deref()
            .or(event.event_type.as_deref())
            .unwrap_or("unknown");

        match kind {
            "gate.pending" | "gate_pending" => {
                let gate_id = event.gate_id.as_deref().unwrap_or("unknown");
                let run_id = event.run_id.as_deref().unwrap_or("?");
                Some(Notification {
                    text: format!(
                        "🚪 Gate approval needed\n\n\
                         Gate: {gate_id}\n\
                         Run: {run_id}"
                    ),
                    buttons: Some(gate_approval_buttons(gate_id)),
                })
            }
            "run.completed" | "run_completed" => {
                let run_id = event.run_id.as_deref().unwrap_or("?");
                let reason = event.reason.as_deref().unwrap_or("completed normally");
                Some(Notification {
                    text: format!("✅ Run {run_id} completed: {reason}"),
                    buttons: None,
                })
            }
            "budget.exceeded" | "budget_exceeded" => {
                let run_id = event.run_id.as_deref().unwrap_or("?");
                Some(Notification {
                    text: format!(
                        "💰 Budget alert for run {run_id}\n\n\
                         Token budget exceeded."
                    ),
                    buttons: None,
                })
            }
            "phase.changed" | "phase_changed" => {
                let run_id = event.run_id.as_deref().unwrap_or("?");
                let phase = event.phase.as_deref().unwrap_or("?");
                Some(Notification {
                    text: format!("📋 Run {run_id} phase → {phase}"),
                    buttons: None,
                })
            }
            _ => {
                debug!(kind, "Skipping unhandled event type");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_event(kind: &str) -> KernelEvent {
        KernelEvent {
            id: Some("evt-001".to_string()),
            event_type: None,
            kind: Some(kind.to_string()),
            run_id: Some("abc123".to_string()),
            phase: Some("execute".to_string()),
            gate_id: Some("gate-review".to_string()),
            reason: Some("all tasks done".to_string()),
            timestamp: Some("2026-02-25T12:00:00Z".to_string()),
            extra: serde_json::Value::Null,
        }
    }

    #[test]
    fn formats_gate_pending() {
        let consumer = EventConsumer::new(
            EventConsumerConfig::default(),
            Arc::new(DemarchAdapter::new(
                intercom_core::config::DemarchConfig::default(),
                ".",
            )),
            Arc::new(crate::ipc::LogOnlyDelegate),
        );

        let notif = consumer
            .format_notification(&test_event("gate.pending"))
            .unwrap();
        assert!(notif.text.contains("Gate approval needed"));
        assert!(notif.text.contains("gate-review"));
        assert!(notif.buttons.is_some());
        let buttons = notif.buttons.unwrap();
        assert_eq!(buttons.inline_keyboard[0].len(), 1);
        assert_eq!(buttons.inline_keyboard[0][0].callback_data, "approve:gate-review");
    }

    #[test]
    fn formats_run_completed() {
        let consumer = EventConsumer::new(
            EventConsumerConfig::default(),
            Arc::new(DemarchAdapter::new(
                intercom_core::config::DemarchConfig::default(),
                ".",
            )),
            Arc::new(crate::ipc::LogOnlyDelegate),
        );

        let notif = consumer
            .format_notification(&test_event("run.completed"))
            .unwrap();
        assert!(notif.text.contains("abc123"));
        assert!(notif.text.contains("all tasks done"));
        assert!(notif.buttons.is_none());
    }

    #[test]
    fn formats_budget_exceeded() {
        let consumer = EventConsumer::new(
            EventConsumerConfig::default(),
            Arc::new(DemarchAdapter::new(
                intercom_core::config::DemarchConfig::default(),
                ".",
            )),
            Arc::new(crate::ipc::LogOnlyDelegate),
        );

        let notif = consumer
            .format_notification(&test_event("budget.exceeded"))
            .unwrap();
        assert!(notif.text.contains("Budget alert"));
        assert!(notif.buttons.is_none());
    }

    #[test]
    fn formats_phase_changed() {
        let consumer = EventConsumer::new(
            EventConsumerConfig::default(),
            Arc::new(DemarchAdapter::new(
                intercom_core::config::DemarchConfig::default(),
                ".",
            )),
            Arc::new(crate::ipc::LogOnlyDelegate),
        );

        let notif = consumer
            .format_notification(&test_event("phase.changed"))
            .unwrap();
        assert!(notif.text.contains("execute"));
        assert!(notif.buttons.is_none());
    }

    #[test]
    fn skips_unknown_events() {
        let consumer = EventConsumer::new(
            EventConsumerConfig::default(),
            Arc::new(DemarchAdapter::new(
                intercom_core::config::DemarchConfig::default(),
                ".",
            )),
            Arc::new(crate::ipc::LogOnlyDelegate),
        );

        assert!(consumer
            .format_notification(&test_event("some.random.event"))
            .is_none());
    }

    #[test]
    fn gate_buttons_have_correct_callback_data() {
        let buttons = gate_approval_buttons("gate-review");
        assert_eq!(buttons.inline_keyboard.len(), 1);
        assert_eq!(buttons.inline_keyboard[0].len(), 1);
        assert_eq!(buttons.inline_keyboard[0][0].callback_data, "approve:gate-review");
    }

    #[test]
    fn parses_event_from_json() {
        let json = r#"{
            "id": "evt-123",
            "kind": "gate.pending",
            "run_id": "run-abc",
            "gate_id": "gate-review",
            "timestamp": "2026-02-25T12:00:00Z"
        }"#;
        let event: KernelEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.kind.as_deref(), Some("gate.pending"));
        assert_eq!(event.gate_id.as_deref(), Some("gate-review"));
    }

    #[test]
    fn parses_events_array() {
        let json = r#"[
            {"id": "1", "kind": "gate.pending", "gate_id": "g1"},
            {"id": "2", "kind": "run.completed", "run_id": "r1"}
        ]"#;
        let events: Vec<KernelEvent> = serde_json::from_str(json).unwrap();
        assert_eq!(events.len(), 2);
    }
}
