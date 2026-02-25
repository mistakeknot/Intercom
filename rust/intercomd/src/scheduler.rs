//! Task scheduler — polls Postgres for due tasks and enqueues them for execution.
//!
//! Port of `src/task-scheduler.ts`. The scheduler runs a poll loop every
//! `poll_interval`, queries `scheduled_tasks` for rows where `next_run <= now()`
//! and `status = 'active'`, and passes them to a callback for container execution.
//!
//! Next-run calculation supports three schedule types:
//! - `cron`: parsed via the `cron` crate with timezone support
//! - `interval`: millisecond offset from now
//! - `once`: no next run (task moves to `completed`)

use std::str::FromStr;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use intercom_core::PgPool;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

/// Configuration for the scheduler loop.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// How often to poll for due tasks.
    pub poll_interval: Duration,
    /// IANA timezone for cron expressions (e.g., "Europe/Berlin").
    pub timezone: String,
    /// Whether the scheduler is enabled.
    pub enabled: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            timezone: "UTC".to_string(),
            enabled: false,
        }
    }
}

/// Callback invoked for each due task. The scheduler passes the task details
/// and expects the callback to enqueue container execution.
pub type TaskCallback = Box<dyn Fn(DueTask) + Send + Sync>;

/// A task that is due for execution.
#[derive(Debug, Clone)]
pub struct DueTask {
    pub id: String,
    pub group_folder: String,
    pub chat_jid: String,
    pub prompt: String,
    pub schedule_type: String,
    pub schedule_value: String,
    pub context_mode: String,
}

/// Calculate the next run time for a task after it completes.
pub fn calculate_next_run(
    schedule_type: &str,
    schedule_value: &str,
    timezone: &str,
) -> Option<String> {
    match schedule_type {
        "cron" => {
            let schedule = match cron::Schedule::from_str(schedule_value) {
                Ok(s) => s,
                Err(e) => {
                    error!(cron = schedule_value, err = %e, "invalid cron expression");
                    return None;
                }
            };
            // Find next occurrence in the configured timezone
            let tz: chrono_tz::Tz = match timezone.parse() {
                Ok(t) => t,
                Err(_) => {
                    warn!(tz = timezone, "invalid timezone, falling back to UTC");
                    chrono_tz::Tz::UTC
                }
            };
            let now = Utc::now().with_timezone(&tz);
            schedule
                .after(&now)
                .next()
                .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
        }
        "interval" => {
            let ms: u64 = match schedule_value.parse() {
                Ok(v) => v,
                Err(e) => {
                    error!(value = schedule_value, err = %e, "invalid interval ms");
                    return None;
                }
            };
            let next = Utc::now() + chrono::Duration::milliseconds(ms as i64);
            Some(next.to_rfc3339())
        }
        "once" => None, // one-shot tasks complete after first run
        other => {
            warn!(schedule_type = other, "unknown schedule type");
            None
        }
    }
}

/// Format a task run result summary for storage.
pub fn result_summary(result: Option<&str>, error: Option<&str>) -> String {
    if let Some(e) = error {
        format!("Error: {e}")
    } else if let Some(r) = result {
        if r.len() > 200 {
            r[..200].to_string()
        } else {
            r.to_string()
        }
    } else {
        "Completed".to_string()
    }
}

/// Run the scheduler poll loop. Exits when `shutdown` signal fires.
pub async fn run_scheduler_loop(
    config: SchedulerConfig,
    pool: PgPool,
    on_task: TaskCallback,
    mut shutdown: watch::Receiver<bool>,
) {
    if !config.enabled {
        info!("scheduler disabled, skipping loop");
        return;
    }
    info!(
        poll_interval_ms = config.poll_interval.as_millis(),
        timezone = %config.timezone,
        "scheduler loop started"
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(config.poll_interval) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("scheduler loop shutting down");
                    return;
                }
            }
        }

        match pool.get_due_tasks().await {
            Ok(tasks) => {
                if !tasks.is_empty() {
                    info!(count = tasks.len(), "found due tasks");
                }
                for task in tasks {
                    // Re-verify status in case it changed between query and processing
                    match pool.get_task_by_id(&task.id).await {
                        Ok(Some(current)) if current.status == "active" => {
                            debug!(task_id = %current.id, group = %current.group_folder, "dispatching task");
                            on_task(DueTask {
                                id: current.id,
                                group_folder: current.group_folder,
                                chat_jid: current.chat_jid,
                                prompt: current.prompt,
                                schedule_type: current.schedule_type,
                                schedule_value: current.schedule_value,
                                context_mode: current.context_mode,
                            });
                        }
                        Ok(Some(_)) => {
                            debug!(task_id = %task.id, "task no longer active, skipping");
                        }
                        Ok(None) => {
                            debug!(task_id = %task.id, "task deleted, skipping");
                        }
                        Err(e) => {
                            error!(task_id = %task.id, err = %e, "failed to re-check task");
                        }
                    }
                }
            }
            Err(e) => {
                error!(err = %e, "failed to query due tasks");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculate_next_run_interval() {
        let next = calculate_next_run("interval", "60000", "UTC");
        assert!(next.is_some());
        // Should be roughly 60 seconds from now
        let ts = next.unwrap();
        assert!(ts.contains('T'));
    }

    #[test]
    fn calculate_next_run_once() {
        let next = calculate_next_run("once", "", "UTC");
        assert!(next.is_none());
    }

    #[test]
    fn calculate_next_run_cron() {
        // Every minute
        let next = calculate_next_run("cron", "0 * * * * *", "UTC");
        assert!(next.is_some());
    }

    #[test]
    fn calculate_next_run_invalid_cron() {
        let next = calculate_next_run("cron", "not a cron", "UTC");
        assert!(next.is_none());
    }

    #[test]
    fn calculate_next_run_invalid_interval() {
        let next = calculate_next_run("interval", "abc", "UTC");
        assert!(next.is_none());
    }

    #[test]
    fn calculate_next_run_unknown_type() {
        let next = calculate_next_run("weekly", "monday", "UTC");
        assert!(next.is_none());
    }

    #[test]
    fn result_summary_error() {
        let s = result_summary(None, Some("connection refused"));
        assert_eq!(s, "Error: connection refused");
    }

    #[test]
    fn result_summary_truncates() {
        let long = "a".repeat(300);
        let s = result_summary(Some(&long), None);
        assert_eq!(s.len(), 200);
    }

    #[test]
    fn result_summary_default() {
        let s = result_summary(None, None);
        assert_eq!(s, "Completed");
    }

    #[test]
    fn result_summary_short() {
        let s = result_summary(Some("Done: 42 items processed"), None);
        assert_eq!(s, "Done: 42 items processed");
    }
}
