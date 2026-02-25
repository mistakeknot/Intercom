//! HTTP endpoints for the Postgres persistence layer.
//!
//! These endpoints let the Node host dual-write to Postgres through
//! intercomd during the migration period. Once Node is retired, the
//! Rust message loop will call PgPool directly.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use intercom_core::persistence::{
    ChatInfo, NewMessage, RegisteredGroup, ScheduledTask, TaskRunLog, TaskUpdate,
};
use intercom_core::PgPool;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Wrapper for error responses from the DB endpoints.
#[derive(Serialize)]
struct DbError {
    error: String,
}

fn db_error(msg: String) -> (StatusCode, Json<DbError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(DbError { error: msg }),
    )
}

fn require_pool(pool: &Option<PgPool>) -> Result<&PgPool, (StatusCode, Json<DbError>)> {
    pool.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(DbError {
                error: "postgres not configured".to_string(),
            }),
        )
    })
}

// ---------------------------------------------------------------------------
// Chat endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct StoreChatMetadataRequest {
    pub jid: String,
    pub timestamp: String,
    pub name: Option<String>,
    pub channel: Option<String>,
    pub is_group: Option<bool>,
}

pub async fn store_chat_metadata(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<StoreChatMetadataRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool
        .store_chat_metadata(
            &req.jid,
            &req.timestamp,
            req.name.as_deref(),
            req.channel.as_deref(),
            req.is_group,
        )
        .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UpdateChatNameRequest {
    pub jid: String,
    pub name: String,
}

pub async fn update_chat_name(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<UpdateChatNameRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.update_chat_name(&req.jid, &req.name).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

pub async fn get_all_chats(State(pool): State<Option<PgPool>>) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_all_chats().await {
        Ok(chats) => (StatusCode::OK, Json(chats)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Message endpoints
// ---------------------------------------------------------------------------

pub async fn store_message(
    State(pool): State<Option<PgPool>>,
    Json(msg): Json<NewMessage>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.store_message(&msg).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct GetNewMessagesRequest {
    pub jids: Vec<String>,
    pub last_timestamp: String,
    pub bot_prefix: String,
}

#[derive(Serialize)]
pub struct GetNewMessagesResponse {
    pub messages: Vec<NewMessage>,
    pub new_timestamp: String,
}

pub async fn get_new_messages(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<GetNewMessagesRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool
        .get_new_messages(&req.jids, &req.last_timestamp, &req.bot_prefix)
        .await
    {
        Ok((messages, new_timestamp)) => (
            StatusCode::OK,
            Json(GetNewMessagesResponse {
                messages,
                new_timestamp,
            }),
        )
            .into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct GetMessagesSinceRequest {
    pub chat_jid: String,
    pub since_timestamp: String,
    pub bot_prefix: String,
}

pub async fn get_messages_since(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<GetMessagesSinceRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool
        .get_messages_since(&req.chat_jid, &req.since_timestamp, &req.bot_prefix)
        .await
    {
        Ok(msgs) => (StatusCode::OK, Json(msgs)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct GetRecentConversationRequest {
    pub chat_jid: String,
    #[serde(default = "default_conversation_limit")]
    pub limit: i64,
}

fn default_conversation_limit() -> i64 {
    20
}

pub async fn get_recent_conversation(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<GetRecentConversationRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool
        .get_recent_conversation(&req.chat_jid, req.limit)
        .await
    {
        Ok(msgs) => (StatusCode::OK, Json(msgs)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Task endpoints
// ---------------------------------------------------------------------------

pub async fn create_task(
    State(pool): State<Option<PgPool>>,
    Json(task): Json<ScheduledTask>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.create_task(&task).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct GetTaskByIdRequest {
    pub id: String,
}

pub async fn get_task_by_id(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<GetTaskByIdRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_task_by_id(&req.id).await {
        Ok(task) => (StatusCode::OK, Json(task)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct GetTasksForGroupRequest {
    pub group_folder: String,
}

pub async fn get_tasks_for_group(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<GetTasksForGroupRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_tasks_for_group(&req.group_folder).await {
        Ok(tasks) => (StatusCode::OK, Json(tasks)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

pub async fn get_all_tasks(State(pool): State<Option<PgPool>>) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_all_tasks().await {
        Ok(tasks) => (StatusCode::OK, Json(tasks)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UpdateTaskRequest {
    pub id: String,
    #[serde(flatten)]
    pub updates: TaskUpdate,
}

pub async fn update_task(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<UpdateTaskRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.update_task(&req.id, &req.updates).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct DeleteTaskRequest {
    pub id: String,
}

pub async fn delete_task(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<DeleteTaskRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.delete_task(&req.id).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

pub async fn get_due_tasks(State(pool): State<Option<PgPool>>) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_due_tasks().await {
        Ok(tasks) => (StatusCode::OK, Json(tasks)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UpdateTaskAfterRunRequest {
    pub id: String,
    pub next_run: Option<String>,
    pub last_result: String,
}

pub async fn update_task_after_run(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<UpdateTaskAfterRunRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool
        .update_task_after_run(&req.id, req.next_run.as_deref(), &req.last_result)
        .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

pub async fn log_task_run(
    State(pool): State<Option<PgPool>>,
    Json(log): Json<TaskRunLog>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.log_task_run(&log).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Router state endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct GetRouterStateRequest {
    pub key: String,
}

pub async fn get_router_state(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<GetRouterStateRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_router_state(&req.key).await {
        Ok(val) => (StatusCode::OK, Json(serde_json::json!({"value": val}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct SetRouterStateRequest {
    pub key: String,
    pub value: String,
}

pub async fn set_router_state(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<SetRouterStateRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.set_router_state(&req.key, &req.value).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Session endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct GetSessionRequest {
    pub group_folder: String,
}

pub async fn get_session(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<GetSessionRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_session(&req.group_folder).await {
        Ok(sid) => (StatusCode::OK, Json(serde_json::json!({"session_id": sid}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct SetSessionRequest {
    pub group_folder: String,
    pub session_id: String,
}

pub async fn set_session(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<SetSessionRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool
        .set_session(&req.group_folder, &req.session_id)
        .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

pub async fn get_all_sessions(State(pool): State<Option<PgPool>>) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_all_sessions().await {
        Ok(sessions) => (StatusCode::OK, Json(sessions)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct DeleteSessionRequest {
    pub group_folder: String,
}

pub async fn delete_session(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<DeleteSessionRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.delete_session(&req.group_folder).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Registered group endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct GetRegisteredGroupRequest {
    pub jid: String,
}

pub async fn get_registered_group(
    State(pool): State<Option<PgPool>>,
    Json(req): Json<GetRegisteredGroupRequest>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_registered_group(&req.jid).await {
        Ok(group) => (StatusCode::OK, Json(group)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

pub async fn set_registered_group(
    State(pool): State<Option<PgPool>>,
    Json(group): Json<RegisteredGroup>,
) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.set_registered_group(&group).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}

pub async fn get_all_registered_groups(State(pool): State<Option<PgPool>>) -> impl IntoResponse {
    let pool = match require_pool(&pool) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match pool.get_all_registered_groups().await {
        Ok(groups) => (StatusCode::OK, Json(groups)).into_response(),
        Err(e) => db_error(e.to_string()).into_response(),
    }
}
