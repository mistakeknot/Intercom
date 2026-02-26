mod commands;
mod container;
mod db;
mod events;
mod ipc;
mod queue;
mod scheduler;
mod telegram;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, anyhow};
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use intercom_compat::{
    LegacyLayout, LegacySnapshot, MigrationOptions, inspect_legacy_layout, inspect_legacy_sqlite,
    migrate_legacy_to_postgres, verify_migration_parity,
};
use intercom_core::{
    DemarchAdapter, DemarchResponse, IntercomConfig, PgPool, ReadOperation, WriteOperation,
    load_config,
};
use serde::{Deserialize, Serialize};
use telegram::{
    TelegramBridge, TelegramEditRequest, TelegramEditResponse, TelegramIngressRequest,
    TelegramIngressResponse, TelegramSendRequest, TelegramSendResponse,
};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "intercomd", version, about = "Intercom Rust daemon skeleton")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start intercomd HTTP service.
    Serve(ServeArgs),
    /// Print effective intercomd config as JSON.
    PrintConfig(PrintConfigArgs),
    /// Inspect legacy Intercom Node/SQLite state for migration planning.
    InspectLegacy(InspectLegacyArgs),
    /// Migrate legacy SQLite state into Postgres (supports dry-run).
    MigrateLegacy(MigrateLegacyArgs),
    /// Compare legacy SQLite counts against migrated Postgres tables.
    VerifyMigration(VerifyMigrationArgs),
}

#[derive(clap::Args, Debug)]
struct ServeArgs {
    #[arg(long, default_value = "config/intercom.toml")]
    config: PathBuf,
    #[arg(long)]
    bind: Option<String>,
}

#[derive(clap::Args, Debug)]
struct PrintConfigArgs {
    #[arg(long, default_value = "config/intercom.toml")]
    config: PathBuf,
}

#[derive(clap::Args, Debug)]
struct InspectLegacyArgs {
    #[arg(long, default_value = "store/messages.db")]
    sqlite: PathBuf,
    #[arg(long, default_value = ".")]
    project_root: PathBuf,
}

#[derive(clap::Args, Debug)]
struct MigrateLegacyArgs {
    #[arg(long, default_value = "store/messages.db")]
    sqlite: PathBuf,
    #[arg(long)]
    postgres_dsn: Option<String>,
    #[arg(long, default_value = "sqlite_to_postgres_v1")]
    checkpoint: String,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = "config/intercom.toml")]
    config: PathBuf,
}

#[derive(clap::Args, Debug)]
struct VerifyMigrationArgs {
    #[arg(long, default_value = "store/messages.db")]
    sqlite: PathBuf,
    #[arg(long)]
    postgres_dsn: Option<String>,
    #[arg(long, default_value = "config/intercom.toml")]
    config: PathBuf,
}

#[derive(Clone)]
struct AppState {
    started_at: Instant,
    config: Arc<IntercomConfig>,
    demarch: Arc<DemarchAdapter>,
    telegram: Arc<TelegramBridge>,
    db: Option<PgPool>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    uptime_seconds: u64,
    bind: String,
}

#[derive(Serialize)]
struct ReadyResponse {
    status: &'static str,
    runtime_profiles: usize,
    demarch_writes_restricted_to_main: bool,
    telegram_bridge_enabled: bool,
    postgres_connected: bool,
}

#[derive(Serialize)]
struct RuntimeProfilesResponse {
    default_runtime: String,
    profiles: Vec<String>,
}

#[derive(Serialize)]
struct LegacyInspectResponse {
    sqlite: PathBuf,
    snapshot: LegacySnapshot,
    layout: LegacyLayout,
}

#[derive(Debug, Deserialize)]
struct DemarchReadRequest {
    #[serde(default)]
    is_main: bool,
    source_group: Option<String>,
    #[serde(flatten)]
    operation: ReadOperation,
}

#[derive(Debug, Deserialize)]
struct DemarchWriteRequest {
    #[serde(default)]
    is_main: bool,
    source_group: Option<String>,
    #[serde(flatten)]
    operation: WriteOperation,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve(ServeArgs {
        config: PathBuf::from("config/intercom.toml"),
        bind: None,
    })) {
        Command::Serve(args) => serve(args).await,
        Command::PrintConfig(args) => print_config(args),
        Command::InspectLegacy(args) => inspect_legacy(args),
        Command::MigrateLegacy(args) => migrate_legacy(args).await,
        Command::VerifyMigration(args) => verify_migration(args).await,
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let mut config = load_config(&args.config)
        .with_context(|| format!("failed to load config from {}", args.config.display()))?;

    if let Some(bind) = args.bind {
        config.server.bind = bind;
    }

    let bind = config.server.bind.clone();
    let host_callback_url = config.server.host_callback_url.clone();
    let project_root =
        std::env::current_dir().context("failed to resolve current working directory")?;
    let demarch = Arc::new(DemarchAdapter::new(config.demarch.clone(), &project_root));
    let telegram = TelegramBridge::new(&config);

    // Connect to Postgres if DSN is configured
    let db = if let Some(ref dsn) = config.storage.postgres_dsn {
        if !dsn.trim().is_empty() {
            let pool = PgPool::new(dsn.clone());
            match pool.connect().await {
                Ok(()) => {
                    info!("postgres persistence layer connected");
                    Some(pool)
                }
                Err(e) => {
                    tracing::warn!(err = %e, "postgres connection failed, DB endpoints disabled");
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let state = AppState {
        started_at: Instant::now(),
        config: Arc::new(config),
        demarch: demarch.clone(),
        telegram: Arc::new(telegram),
        db,
    };

    // IPC watcher — polls data/ipc/ directories for container messages/queries
    let ipc_config = ipc::IpcWatcherConfig {
        ipc_base_dir: project_root.join("data/ipc"),
        ..Default::default()
    };
    let delegate: Arc<dyn ipc::IpcDelegate> =
        Arc::new(ipc::HttpDelegate::new(&host_callback_url));
    let registry = ipc::GroupRegistry::new();
    info!(
        host_callback_url = %host_callback_url,
        "IPC delegate: forwarding messages/tasks to Node host"
    );
    let ipc_watcher =
        ipc::IpcWatcher::with_registry(ipc_config, demarch, delegate, registry.clone());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let ipc_shutdown_rx = shutdown_rx.clone();
    let ipc_handle = tokio::spawn(async move {
        ipc_watcher.run(ipc_shutdown_rx).await;
    });

    // Group registry sync — fetches registered groups from Node host periodically
    let registry_shutdown_rx = shutdown_rx.clone();
    let registry_url = host_callback_url.clone();
    let registry_handle = tokio::spawn(async move {
        ipc::sync_registry_loop(registry, registry_url, registry_shutdown_rx).await;
    });

    // Event consumer — polls ic events tail and sends push notifications
    let events_config = events::EventConsumerConfig {
        poll_interval: std::time::Duration::from_millis(
            state.config.events.poll_interval_ms,
        ),
        batch_size: state.config.events.batch_size,
        notification_jid: state.config.events.notification_jid.clone(),
        enabled: state.config.events.enabled,
    };
    let events_demarch = state.demarch.clone();
    let events_delegate: Arc<dyn ipc::IpcDelegate> =
        Arc::new(ipc::HttpDelegate::new(&host_callback_url));
    let events_shutdown_rx = shutdown_rx.clone();
    let events_handle = tokio::spawn(async move {
        let mut consumer =
            events::EventConsumer::new(events_config, events_demarch, events_delegate);
        consumer.run(events_shutdown_rx).await;
    });

    // DB routes use Option<PgPool> state — nested router avoids exposing
    // full AppState to the db module.
    let db_routes = Router::new()
        .route("/chats", post(db::store_chat_metadata))
        .route("/chats/name", post(db::update_chat_name))
        .route("/chats/all", post(db::get_all_chats))
        .route("/messages", post(db::store_message))
        .route("/messages/new", post(db::get_new_messages))
        .route("/messages/since", post(db::get_messages_since))
        .route("/messages/conversation", post(db::get_recent_conversation))
        .route("/tasks", post(db::create_task))
        .route("/tasks/get", post(db::get_task_by_id))
        .route("/tasks/group", post(db::get_tasks_for_group))
        .route("/tasks/all", post(db::get_all_tasks))
        .route("/tasks/update", post(db::update_task))
        .route("/tasks/delete", post(db::delete_task))
        .route("/tasks/due", post(db::get_due_tasks))
        .route("/tasks/after-run", post(db::update_task_after_run))
        .route("/tasks/log", post(db::log_task_run))
        .route("/router-state/get", post(db::get_router_state))
        .route("/router-state/set", post(db::set_router_state))
        .route("/sessions/get", post(db::get_session))
        .route("/sessions/set", post(db::set_session))
        .route("/sessions/all", post(db::get_all_sessions))
        .route("/sessions/delete", post(db::delete_session))
        .route("/groups/get", post(db::get_registered_group))
        .route("/groups/set", post(db::set_registered_group))
        .route("/groups/all", post(db::get_all_registered_groups))
        .with_state(state.db.clone());

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/runtime/profiles", get(runtime_profiles))
        .route("/v1/demarch/read", post(demarch_read))
        .route("/v1/demarch/write", post(demarch_write))
        .route("/v1/telegram/ingress", post(telegram_ingress))
        .route("/v1/telegram/send", post(telegram_send))
        .route("/v1/telegram/edit", post(telegram_edit))
        .route("/v1/commands", post(handle_slash_command))
        .nest("/v1/db", db_routes)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind listener on {bind}"))?;

    info!(bind = %bind, "intercomd listening (IPC watcher active)");
    let result = axum::serve(listener, app)
        .await
        .context("server exited unexpectedly");

    // Signal background tasks to stop on server exit
    let _ = shutdown_tx.send(true);
    let _ = ipc_handle.await;
    let _ = registry_handle.await;
    let _ = events_handle.await;

    result
}

fn print_config(args: PrintConfigArgs) -> anyhow::Result<()> {
    let cfg = load_config(&args.config)
        .with_context(|| format!("failed to load config from {}", args.config.display()))?;
    println!("{}", serde_json::to_string_pretty(&cfg)?);
    Ok(())
}

fn inspect_legacy(args: InspectLegacyArgs) -> anyhow::Result<()> {
    let snapshot = inspect_legacy_sqlite(&args.sqlite)
        .with_context(|| format!("failed to inspect sqlite file {}", args.sqlite.display()))?;
    let layout = inspect_legacy_layout(&args.project_root);
    let response = LegacyInspectResponse {
        sqlite: args.sqlite,
        snapshot,
        layout,
    };

    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn migrate_legacy(args: MigrateLegacyArgs) -> anyhow::Result<()> {
    let postgres_dsn = if args.dry_run {
        args.postgres_dsn.unwrap_or_default()
    } else {
        resolve_postgres_dsn(args.postgres_dsn, &args.config)?
    };

    let report = migrate_legacy_to_postgres(MigrationOptions {
        sqlite_path: args.sqlite,
        postgres_dsn,
        dry_run: args.dry_run,
        checkpoint_name: args.checkpoint,
    })
    .await?;

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn verify_migration(args: VerifyMigrationArgs) -> anyhow::Result<()> {
    let postgres_dsn = resolve_postgres_dsn(args.postgres_dsn, &args.config)?;
    let report = verify_migration_parity(args.sqlite, &postgres_dsn).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn resolve_postgres_dsn(explicit: Option<String>, config_path: &PathBuf) -> anyhow::Result<String> {
    if let Some(dsn) = explicit {
        if !dsn.trim().is_empty() {
            return Ok(dsn);
        }
    }

    let config = load_config(config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))?;
    if let Some(dsn) = config.storage.postgres_dsn {
        if !dsn.trim().is_empty() {
            return Ok(dsn);
        }
    }

    Err(anyhow!(
        "Postgres DSN is required. Set --postgres-dsn, INTERCOM_POSTGRES_DSN, or storage.postgres_dsn in config."
    ))
}

async fn healthz(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "intercomd",
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        bind: state.config.server.bind.clone(),
    })
}

async fn readyz(State(state): State<AppState>) -> Json<ReadyResponse> {
    Json(ReadyResponse {
        status: "ready",
        runtime_profiles: state.config.runtimes.profiles.len(),
        demarch_writes_restricted_to_main: state.config.demarch.require_main_group_for_writes,
        telegram_bridge_enabled: state.telegram.is_enabled(),
        postgres_connected: state.db.is_some(),
    })
}

async fn runtime_profiles(State(state): State<AppState>) -> Json<RuntimeProfilesResponse> {
    let mut profiles = state
        .config
        .runtimes
        .profiles
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    profiles.sort();

    Json(RuntimeProfilesResponse {
        default_runtime: state.config.runtimes.default_runtime.clone(),
        profiles,
    })
}

async fn demarch_read(
    State(state): State<AppState>,
    Json(request): Json<DemarchReadRequest>,
) -> Json<DemarchResponse> {
    let _ = request.source_group;
    let _ = request.is_main;
    Json(state.demarch.execute_read(request.operation))
}

async fn demarch_write(
    State(state): State<AppState>,
    Json(request): Json<DemarchWriteRequest>,
) -> Json<DemarchResponse> {
    let _ = request.source_group;
    Json(
        state
            .demarch
            .execute_write(request.operation, request.is_main),
    )
}

async fn telegram_ingress(
    State(state): State<AppState>,
    Json(request): Json<TelegramIngressRequest>,
) -> Json<TelegramIngressResponse> {
    match state.telegram.route_ingress(&state.config, request) {
        Ok(response) => Json(response),
        Err(err) => Json(TelegramIngressResponse {
            accepted: false,
            reason: Some(format!("routing_error: {err}")),
            normalized_content: String::new(),
            group_name: None,
            group_folder: None,
            runtime: None,
            model: None,
            parity: telegram::TelegramIngressParity {
                trigger_required: false,
                trigger_present: false,
                runtime_profile_found: false,
                runtime_fallback_used: false,
                model_fallback_used: false,
            },
        }),
    }
}

async fn telegram_send(
    State(state): State<AppState>,
    Json(request): Json<TelegramSendRequest>,
) -> Json<TelegramSendResponse> {
    match state.telegram.send_message(request).await {
        Ok(response) => Json(response),
        Err(err) => Json(TelegramSendResponse::from_error(err.to_string())),
    }
}

async fn telegram_edit(
    State(state): State<AppState>,
    Json(request): Json<TelegramEditRequest>,
) -> Json<TelegramEditResponse> {
    match state.telegram.edit_message(request).await {
        Ok(response) => Json(response),
        Err(err) => Json(TelegramEditResponse::from_error(err.to_string())),
    }
}

async fn handle_slash_command(
    State(state): State<AppState>,
    Json(request): Json<commands::CommandRequest>,
) -> Json<commands::CommandResult> {
    let assistant_name = std::env::var("ASSISTANT_NAME")
        .unwrap_or_else(|_| "Amtiskaw".into());
    let ctx = commands::CommandContext {
        assistant_name,
        started_at: state.started_at,
    };
    let result = commands::handle_command(
        &request.command,
        &request.args,
        request.group_name.as_deref(),
        request.group_folder.as_deref(),
        request.current_model.as_deref(),
        request.session_id.as_deref(),
        request.container_active,
        &ctx,
    );
    Json(result)
}
