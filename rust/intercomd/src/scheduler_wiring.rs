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
use std::time::{Duration, Instant};

use intercom_core::{
    ContainerInput, ContainerOutput, ContainerStatus, NewMessage, PgPool, PoolConfig,
    RegisteredGroup, RuntimeConfig,
};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::container::mounts::GroupInfo;
use crate::container::runner::{RunConfig, pool_spawn, run_container_agent, write_snapshots};
use crate::container::security::ContainerConfig;
use crate::process_group::resolve_execution_profile;
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
    runtime_config: RuntimeConfig,
    run_config: RunConfig,
    timezone: String,
    pool_config: PoolConfig,
) -> TaskCallback {
    Box::new(move |task: DueTask| {
        let pool = pool.clone();
        let queue = queue.clone();
        let groups = groups.clone();
        let sessions = sessions.clone();
        let telegram = telegram.clone();
        let runtime_config = runtime_config.clone();
        let run_config = run_config.clone();
        let timezone = timezone.clone();
        let pool_config = pool_config.clone();

        let task_id = task.id.clone();
        let chat_jid = task.chat_jid.clone();

        // Clone queue before moving it into the task_fn closure
        let queue_for_enqueue = queue.clone();

        let task_fn = Box::new(
            move || -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
                Box::pin(async move {
                    run_scheduled_task(
                        task,
                        &pool,
                        &queue,
                        &groups,
                        &sessions,
                        &telegram,
                        &runtime_config,
                        &run_config,
                        &timezone,
                        &pool_config,
                    )
                    .await;
                })
            },
        );

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
    runtime_config: &RuntimeConfig,
    run_config: &RunConfig,
    timezone: &str,
    pool_config: &PoolConfig,
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

    let execution_profile = resolve_execution_profile(&group, runtime_config);
    let runtime = execution_profile.runtime;

    // Clone before assistant_name is moved into ContainerInput
    let assistant_name_cb = assistant_name.clone();
    let assistant_name_warm = assistant_name.clone();

    let input = ContainerInput {
        prompt: task.prompt.clone(),
        session_id,
        group_folder: task.group_folder.clone(),
        chat_jid: task.chat_jid.clone(),
        is_main,
        is_scheduled_task: Some(true),
        assistant_name: Some(assistant_name),
        model: execution_profile.model,
        reasoning_effort: execution_profile.reasoning_effort,
        service_tier: execution_profile.service_tier,
        secrets: None,
        previous_context: None,
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
    let task_id_cb = task.id.clone();

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
            let task_id_cb = task_id_cb.clone();
            let assistant_name_cb = assistant_name_cb.clone();
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

                // Persist before deliver: store output in Postgres first so it
                // survives delivery failures or process crashes
                if let Some(ref text) = output.result {
                    if !text.is_empty() {
                        let bot_msg = intercom_core::NewMessage {
                            id: format!("task-{}-{}", task_id_cb, chrono::Utc::now().timestamp_millis()),
                            chat_jid: chat_jid.clone(),
                            sender: "bot".into(),
                            sender_name: assistant_name_cb.clone(),
                            content: text.clone(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            is_from_me: true,
                            is_bot_message: true,
                        };
                        if let Err(e) = pool.store_message(&bot_msg).await {
                            warn!(err = %e, "failed to persist task output");
                        }

                        // Deliver via Telegram
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

                // Notify queue on completion — only on true idle signal
                // (not model announcements or session updates)
                if output.status == ContainerStatus::Success
                    && output.result.is_none()
                    && output.model.is_none()
                    && output.new_session_id.is_none()
                {
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

    // Try warm container path first (if pool is enabled and container is warm)
    if pool_config.enabled && queue.has_warm_container(&task.chat_jid).await {
        info!(task_id = task.id.as_str(), group = group.name.as_str(), "using warm container for scheduled task");

        if let Some(mut delivery_rx) = queue.send_to_warm_container(&task.chat_jid, &task.prompt).await {
            let sessions_cb = sessions.clone();
            let pool_cb = pool.clone();
            let queue_cb = queue.clone();
            let chat_jid = task.chat_jid.clone();
            let group_folder = task.group_folder.clone();
            let task_id_cb = task.id.clone();
            let assistant_name = assistant_name_warm.clone();

            let mut warm_result: Option<String> = None;
            let mut warm_error: Option<String> = None;

            while let Some(output) = delivery_rx.recv().await {
                if let Some(ref sid) = output.new_session_id {
                    let mut s = sessions_cb.write().await;
                    s.insert(group_folder.clone(), sid.clone());
                    if let Err(e) = pool_cb.set_session(&group_folder, sid).await {
                        warn!(err = %e, "failed to persist session");
                    }
                }

                if let Some(ref text) = output.result {
                    if !text.is_empty() {
                        let bot_msg = NewMessage {
                            id: format!("task-{}-{}", task_id_cb, chrono::Utc::now().timestamp_millis()),
                            chat_jid: chat_jid.clone(),
                            sender: "bot".into(),
                            sender_name: assistant_name.clone(),
                            content: text.clone(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            is_from_me: true,
                            is_bot_message: true,
                        };
                        if let Err(e) = pool_cb.store_message(&bot_msg).await {
                            warn!(err = %e, "failed to persist task output (warm)");
                        }
                        if let Err(e) = telegram.send_text_to_jid(&chat_jid, text).await {
                            error!(err = %e, "failed to send task output via Telegram (warm)");
                        }
                        warm_result = Some(text.clone());
                    }
                }

                if output.status == ContainerStatus::Error {
                    warm_error = Some(output.error.clone().unwrap_or_else(|| "Unknown error".into()));
                }

                // True idle signal: Success with no result, model, or session.
                if output.status == ContainerStatus::Success
                    && output.result.is_none()
                    && output.model.is_none()
                    && output.new_session_id.is_none()
                {
                    queue_cb.notify_idle(&chat_jid).await;
                    break;
                }
            }

            log_and_update(pool, &task, start, warm_result.as_deref(), warm_error.as_deref(), timezone).await;
            return;
        }
        // send_to_warm_container returned None — fall through to cold path
        warn!(task_id = task.id.as_str(), "warm container send failed for scheduled task, falling back");
    }

    let on_spawn: Option<Arc<crate::container::runner::OnSpawnCallback>> = {
        let q = queue.clone();
        let jid = task.chat_jid.clone();
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

    let container_result = run_container_agent(
        &group_info,
        &input,
        runtime,
        is_main,
        run_config,
        on_output,
        on_spawn,
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

/// Log the task run and update next_run in Postgres (single transaction).
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

    let log = intercom_core::TaskRunLog {
        task_id: task.id.clone(),
        run_at: chrono::Utc::now().to_rfc3339(),
        duration_ms,
        status: status.into(),
        result: result.map(|s| s.to_string()),
        error: error.map(|s| s.to_string()),
    };

    let next_run = calculate_next_run(&task.schedule_type, &task.schedule_value, timezone);
    let summary = result_summary(result, error);

    const DB_TIMEOUT: Duration = Duration::from_secs(30);
    match tokio::time::timeout(
        DB_TIMEOUT,
        pool.log_and_update_task(&log, &task.id, next_run.as_deref(), &summary),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            error!(task_id = task.id.as_str(), err = %e, "failed to log and update task (transaction)");
        }
        Err(_elapsed) => {
            error!(task_id = task.id.as_str(), timeout_secs = DB_TIMEOUT.as_secs(), "DB timeout in log_and_update_task");
        }
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
