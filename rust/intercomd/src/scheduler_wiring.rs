//! Scheduler → GroupQueue wiring.
//!
//! Builds the `TaskCallback` closure that the scheduler loop invokes for each
//! due task. The callback enqueues a `TaskFn` into `GroupQueue` that:
//! 1. Resolves group and session state
//! 2. Runs `run_container_agent()` with the task prompt
//! 3. Sends output to Telegram
//! 4. Logs the run and advances next_run in Postgres

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use intercom_core::{ContainerInput, ContainerOutput, ContainerStatus, PgPool, RegisteredGroup};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::container::mounts::GroupInfo;
use crate::container::runner::{RunConfig, run_container_agent, write_snapshots};
use crate::container::security::ContainerConfig;
use crate::process_group::resolve_runtime;
use crate::queue::GroupQueue;
use crate::scheduler::{DueTask, TaskCallback, calculate_next_run, result_summary};
use crate::telegram::TelegramBridge;

/// Build the `TaskCallback` that the scheduler loop invokes for each due task.
///
/// The callback captures all shared state and enqueues a `TaskFn` into the
/// `GroupQueue` for per-group serialized execution.
pub fn build_task_callback(
    pool: PgPool,
    queue: Arc<GroupQueue>,
    groups: Arc<RwLock<HashMap<String, RegisteredGroup>>>,
    sessions: Arc<RwLock<HashMap<String, String>>>,
    telegram: Arc<TelegramBridge>,
    run_config: RunConfig,
    timezone: String,
) -> TaskCallback {
    Box::new(move |task: DueTask| {
        let pool = pool.clone();
        let queue = queue.clone();
        let groups = groups.clone();
        let sessions = sessions.clone();
        let telegram = telegram.clone();
        let run_config = run_config.clone();
        let timezone = timezone.clone();

        let task_id = task.id.clone();
        let chat_jid = task.chat_jid.clone();

        // Clone queue before moving it into the task_fn closure
        let queue_for_enqueue = queue.clone();

        let task_fn = Box::new(move || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(async move {
                run_scheduled_task(
                    task, &pool, &queue, &groups, &sessions, &telegram, &run_config, &timezone,
                )
                .await;
            })
        });

        // Fire-and-forget: enqueue_task is async, so spawn a small task to call it
        tokio::spawn(async move {
            queue_for_enqueue.enqueue_task(&chat_jid, &task_id, task_fn).await;
        });
    })
}

/// Execute a single scheduled task inside a container.
async fn run_scheduled_task(
    task: DueTask,
    pool: &PgPool,
    queue: &Arc<GroupQueue>,
    groups: &Arc<RwLock<HashMap<String, RegisteredGroup>>>,
    sessions: &Arc<RwLock<HashMap<String, String>>>,
    telegram: &Arc<TelegramBridge>,
    run_config: &RunConfig,
    timezone: &str,
) {
    let start = Instant::now();
    let assistant_name = std::env::var("ASSISTANT_NAME").unwrap_or_else(|_| "Amtiskaw".into());

    // Look up group
    let group = {
        let g = groups.read().await;
        match g.values().find(|g| g.folder == task.group_folder) {
            Some(group) => group.clone(),
            None => {
                error!(
                    task_id = task.id.as_str(),
                    group_folder = task.group_folder.as_str(),
                    "scheduled task references unknown group folder"
                );
                log_and_update(pool, &task, start, None, Some("Unknown group folder"), timezone).await;
                return;
            }
        }
    };

    let is_main = false; // scheduled tasks are never "main group" in practice

    // Resolve session based on context_mode
    let session_id = if task.context_mode == "group" {
        let s = sessions.read().await;
        s.get(&task.group_folder).cloned()
    } else {
        None // isolated tasks get a fresh session
    };

    let runtime = resolve_runtime(&group);

    let input = ContainerInput {
        prompt: task.prompt.clone(),
        session_id,
        group_folder: task.group_folder.clone(),
        chat_jid: task.chat_jid.clone(),
        is_main,
        is_scheduled_task: Some(true),
        assistant_name: Some(assistant_name),
        model: group.model.clone(),
        secrets: None,
    };

    let group_info = GroupInfo {
        folder: group.folder.clone(),
        name: group.name.clone(),
        container_config: group
            .container_config
            .as_ref()
            .and_then(|v| serde_json::from_value::<ContainerConfig>(v.clone()).ok()),
    };

    // Output callback — sends results to Telegram, tracks session
    let telegram_cb = telegram.clone();
    let sessions_cb = sessions.clone();
    let pool_cb = pool.clone();
    let queue_cb = queue.clone();
    let chat_jid_cb = task.chat_jid.clone();
    let group_folder_cb = task.group_folder.clone();

    let result_text: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    let error_text: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    let result_cb = result_text.clone();
    let error_cb = error_text.clone();

    let on_output: Option<Arc<crate::container::runner::OutputCallback>> = Some(Arc::new(Box::new(
        move |output: ContainerOutput| {
            let telegram = telegram_cb.clone();
            let sessions = sessions_cb.clone();
            let pool = pool_cb.clone();
            let queue = queue_cb.clone();
            let chat_jid = chat_jid_cb.clone();
            let group_folder = group_folder_cb.clone();
            let result_cb = result_cb.clone();
            let error_cb = error_cb.clone();

            Box::pin(async move {
                // Track session
                if let Some(ref sid) = output.new_session_id {
                    let mut s = sessions.write().await;
                    s.insert(group_folder.clone(), sid.clone());
                    if let Err(e) = pool.set_session(&group_folder, sid).await {
                        warn!(err = %e, "failed to persist session");
                    }
                }

                // Send results to user
                if let Some(ref text) = output.result {
                    if !text.is_empty() {
                        if let Err(e) = telegram.send_text_to_jid(&chat_jid, text).await {
                            error!(err = %e, "failed to send task output via Telegram");
                        }
                        *result_cb.write().await = Some(text.clone());
                    }
                }

                // Track errors
                if output.status == ContainerStatus::Error {
                    let err_msg = output.error.clone().unwrap_or_else(|| "Unknown error".into());
                    *error_cb.write().await = Some(err_msg);
                }

                // Notify queue on completion
                if output.status == ContainerStatus::Success {
                    queue.notify_idle(&chat_jid).await;
                }
            })
        },
    )));

    // Write task/group snapshots for container consumption
    {
        let tasks_json = match pool.get_all_tasks().await {
            Ok(tasks) => {
                let filtered: Vec<_> = tasks.into_iter()
                    .filter(|t| t.group_folder == task.group_folder)
                    .collect();
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
        write_snapshots(&run_config.data_dir, &task.group_folder, is_main, &tasks_json, &groups_json).await;
    }

    info!(
        task_id = task.id.as_str(),
        group = group.name.as_str(),
        "running scheduled task"
    );

    let container_result = run_container_agent(
        &group_info,
        &input,
        runtime,
        is_main,
        run_config,
        on_output,
    )
    .await;

    // Collect final state
    let result = result_text.read().await.clone();
    let error = error_text.read().await.clone();

    let (final_result, final_error) = match container_result {
        Ok(run_result) => {
            // Track session from final output
            if let Some(ref sid) = run_result.output.new_session_id {
                let mut s = sessions.write().await;
                s.insert(task.group_folder.clone(), sid.clone());
                if let Err(e) = pool.set_session(&task.group_folder, sid).await {
                    warn!(err = %e, "failed to persist session");
                }
            }

            if run_result.output.status == ContainerStatus::Error {
                let err = error.or_else(|| run_result.output.error.clone())
                    .unwrap_or_else(|| "Unknown error".into());
                (result, Some(err))
            } else {
                (result.or(run_result.output.result), None)
            }
        }
        Err(e) => {
            error!(task_id = task.id.as_str(), err = %e, "task container error");
            (result, Some(e.to_string()))
        }
    };

    log_and_update(pool, &task, start, final_result.as_deref(), final_error.as_deref(), timezone).await;
}

/// Log the task run and update next_run in Postgres.
async fn log_and_update(
    pool: &PgPool,
    task: &DueTask,
    start: Instant,
    result: Option<&str>,
    error: Option<&str>,
    timezone: &str,
) {
    let duration_ms = start.elapsed().as_millis() as i64;
    let status = if error.is_some() { "error" } else { "success" };

    // Log run
    let log = intercom_core::TaskRunLog {
        task_id: task.id.clone(),
        run_at: chrono::Utc::now().to_rfc3339(),
        duration_ms,
        status: status.into(),
        result: result.map(|s| s.to_string()),
        error: error.map(|s| s.to_string()),
    };
    if let Err(e) = pool.log_task_run(&log).await {
        error!(task_id = task.id.as_str(), err = %e, "failed to log task run");
    }

    // Calculate and set next_run
    let next_run = calculate_next_run(&task.schedule_type, &task.schedule_value, timezone);
    let summary = result_summary(result, error);

    if let Err(e) = pool
        .update_task_after_run(&task.id, next_run.as_deref(), &summary)
        .await
    {
        error!(task_id = task.id.as_str(), err = %e, "failed to update task after run");
    }

    info!(
        task_id = task.id.as_str(),
        status,
        duration_ms,
        next_run = next_run.as_deref().unwrap_or("none"),
        "scheduled task completed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_summary_delegates() {
        assert_eq!(result_summary(Some("ok"), None), "ok");
        assert_eq!(result_summary(None, Some("fail")), "Error: fail");
    }
}
