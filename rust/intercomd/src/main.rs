mod commands;
mod container;
mod db;
mod events;
mod ipc;
mod message_loop;
mod outbox;
mod process_group;
mod queue;
mod scheduler;
mod scheduler_wiring;
mod telegram;
mod telegram_poller;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, anyhow};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use intercom_compat::{
    LegacyLayout, LegacySnapshot, MigrationOptions, inspect_legacy_layout, inspect_legacy_sqlite,
    migrate_legacy_to_postgres, verify_migration_parity,
};
use intercom_core::{
    DemarchAdapter, DemarchResponse, IntercomConfig, PgPool, ReadOperation, RegisteredGroup,
    WriteOperation, load_config,
};
use serde::{Deserialize, Serialize};
use telegram::{
    TelegramBridge, TelegramCallbackRequest, TelegramCallbackResponse, TelegramEditRequest,
    TelegramEditResponse, TelegramIngressRequest, TelegramIngressResponse, TelegramSendRequest,
    TelegramSendResponse,
};
use tokio::sync::RwLock;
use tracing::{info, warn};

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

/// Shared orchestrator state: registered groups indexed by JID.
type Groups = HashMap<String, RegisteredGroup>;
/// Shared session state: group folder → session ID.
type Sessions = HashMap<String, String>;

#[derive(Clone)]
struct AppState {
    started_at: Instant,
    config: Arc<IntercomConfig>,
    demarch: Arc<DemarchAdapter>,
    telegram: Arc<TelegramBridge>,
    db: Option<PgPool>,
    queue: Arc<queue::GroupQueue>,
    groups: Arc<RwLock<Groups>>,
    sessions: Arc<RwLock<Sessions>>,
    agent_timestamps: Arc<RwLock<message_loop::AgentTimestamps>>,
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
    orchestrator_enabled: bool,
    use_outbox: bool,
    registered_groups: usize,
    active_containers: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    outbox_pending: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outbox_failed: Option<i64>,
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

    // Initialize orchestrator state
    let queue = Arc::new(queue::GroupQueue::new(
        config.orchestrator.max_concurrent_containers,
        project_root.join("data"),
    ));

    // Load registered groups and sessions from Postgres (if available)
    let (groups, sessions) = if let Some(ref pool) = db {
        let g = match pool.get_all_registered_groups().await {
            Ok(g) => {
                info!(count = g.len(), "loaded registered groups from Postgres");
                g
            }
            Err(e) => {
                tracing::warn!(err = %e, "failed to load groups, starting empty");
                HashMap::new()
            }
        };
        let s = match pool.get_all_sessions().await {
            Ok(s) => {
                info!(count = s.len(), "loaded sessions from Postgres");
                s
            }
            Err(e) => {
                tracing::warn!(err = %e, "failed to load sessions, starting empty");
                HashMap::new()
            }
        };
        (g, s)
    } else {
        (HashMap::new(), HashMap::new())
    };

    let groups = Arc::new(RwLock::new(groups));
    let sessions = Arc::new(RwLock::new(sessions));

    // Load agent timestamps from Postgres (or start empty)
    let agent_timestamps = if let Some(ref pool) = db {
        Arc::new(RwLock::new(message_loop::load_agent_timestamps_pub(pool).await))
    } else {
        Arc::new(RwLock::new(message_loop::AgentTimestamps::default()))
    };

    let state = AppState {
        started_at: Instant::now(),
        config: Arc::new(config),
        demarch: demarch.clone(),
        telegram: Arc::new(telegram),
        db,
        queue,
        groups,
        sessions,
        agent_timestamps,
    };

    // Determine IPC delegate mode: Telegram-direct (no Node) or HTTP (Node host)
    let telegram_only = state.telegram.is_enabled()
        && std::env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .is_some();

    // IPC watcher — polls data/ipc/ directories for container messages/queries
    let ipc_config = ipc::IpcWatcherConfig {
        ipc_base_dir: project_root.join("data/ipc"),
        ..Default::default()
    };
    let delegate: Arc<dyn ipc::IpcDelegate> = if telegram_only {
        info!("IPC delegate: sending via TelegramBridge directly (no Node host)");
        Arc::new(ipc::TelegramDelegate::new(state.telegram.clone()))
    } else {
        info!(
            host_callback_url = %host_callback_url,
            "IPC delegate: forwarding messages/tasks to Node host"
        );
        Arc::new(ipc::HttpDelegate::new(&host_callback_url))
    };
    let registry = ipc::GroupRegistry::with_fallback(state.groups.clone());
    let ipc_watcher =
        ipc::IpcWatcher::with_registry(ipc_config, demarch, delegate.clone(), registry.clone());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let ipc_shutdown_rx = shutdown_rx.clone();
    let ipc_handle = tokio::spawn(async move {
        ipc_watcher.run(ipc_shutdown_rx).await;
    });

    // Group registry sync — only needed when Node host is running
    let registry_handle = if !telegram_only {
        let registry_shutdown_rx = shutdown_rx.clone();
        let registry_url = host_callback_url.clone();
        Some(tokio::spawn(async move {
            ipc::sync_registry_loop(registry, registry_url, registry_shutdown_rx).await;
        }))
    } else {
        info!("Group registry sync disabled (Telegram-only mode, groups from Postgres)");
        None
    };

    // Event consumer — polls ic events tail and sends push notifications
    let events_config = events::EventConsumerConfig {
        poll_interval: std::time::Duration::from_millis(
            state.config.events.poll_interval_ms,
        ),
        batch_size: state.config.events.batch_size,
        notification_jid: state.config.events.notification_jid.clone(),
        enabled: state.config.events.enabled,
        stale_phase_threshold_secs: state.config.events.stale_phase_threshold_secs,
    };
    let events_demarch = state.demarch.clone();
    let events_delegate = delegate;
    let events_shutdown_rx = shutdown_rx.clone();
    let events_handle = tokio::spawn(async move {
        let mut consumer =
            events::EventConsumer::new(events_config, events_demarch, events_delegate);
        consumer.run(events_shutdown_rx).await;
    });

    // Orchestrator loops (message poll + scheduler) — behind feature flag
    let mut scheduler_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut message_loop_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut outbox_drain_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut outbox_listen_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut outbox_cleanup_handle: Option<tokio::task::JoinHandle<()>> = None;

    if state.config.orchestrator.enabled {
        if let Some(ref pool) = state.db {
            let run_config = container::runner::RunConfig {
                project_root: project_root.clone(),
                groups_dir: project_root.join("groups"),
                data_dir: project_root.join("data"),
                timezone: state.config.scheduler.timezone.clone(),
                idle_timeout_ms: state.config.orchestrator.idle_timeout_ms,
                allowlist: container::security::load_allowlist(
                    &container::security::default_allowlist_path(),
                ),
            };

            let assistant_name = std::env::var("ASSISTANT_NAME")
                .unwrap_or_else(|_| "Amtiskaw".into());

            // Wire processGroupMessages callback into the queue
            let process_fn = process_group::build_process_messages_fn(
                pool.clone(),
                state.queue.clone(),
                state.groups.clone(),
                state.sessions.clone(),
                state.agent_timestamps.clone(),
                state.telegram.clone(),
                assistant_name.clone(),
                state.config.orchestrator.main_group_folder.clone(),
                run_config.clone(),
            );
            state.queue.set_process_messages_fn(process_fn).await;

            // Startup recovery: check for messages that arrived but were never
            // dispatched to an agent (e.g. intercomd crashed mid-container).
            // The outbox stale recovery handles message_outbox rows, but this
            // handles the gap between "message stored in messages table" and
            // "agent actually processed it" — GroupQueue state is in-memory and
            // lost on crash.
            {
                let groups_snap = state.groups.read().await;
                let ts = state.agent_timestamps.read().await;
                let mut recovered = 0u32;
                for (jid, _group) in groups_snap.iter() {
                    let cursor = ts.0.get(jid).cloned().unwrap_or_default();
                    match pool.count_pending_messages(jid, &cursor, &assistant_name).await {
                        Ok(count) if count > 0 => {
                            info!(group_jid = %jid, pending = count, "recovering unprocessed messages from prior crash");
                            state.queue.enqueue_message_check(jid).await;
                            recovered += 1;
                        }
                        Ok(_) => {}
                        Err(e) => warn!(group_jid = %jid, err = %e, "startup pending message check failed"),
                    }
                }
                if recovered > 0 {
                    info!(groups_recovered = recovered, "startup message recovery complete");
                }
            }

            if state.config.orchestrator.use_outbox && !telegram_only {
                // Outbox mode: drain loop + LISTEN/NOTIFY
                // Only needed when Node writes to message_outbox. In Telegram-only
                // mode, the Rust poller writes directly to messages + enqueues.
                info!("outbox mode enabled — spawning drain + LISTEN loops");

                let (drain_tx, drain_rx) = tokio::sync::mpsc::channel::<()>(16);

                // Outbox drain loop
                let drain_pool = pool.clone();
                let drain_queue = state.queue.clone();
                let drain_groups = state.groups.clone();
                let drain_shutdown = shutdown_rx.clone();
                outbox_drain_handle = Some(tokio::spawn(async move {
                    outbox::run_outbox_drain(drain_pool, drain_queue, drain_groups, drain_rx, drain_shutdown)
                        .await;
                }));

                // LISTEN/NOTIFY loop (dedicated connection — DSN from config, not pool)
                let listen_dsn = state.config.storage.postgres_dsn.clone().unwrap_or_default();
                let listen_shutdown = shutdown_rx.clone();
                outbox_listen_handle = Some(tokio::spawn(async move {
                    outbox::run_listen_loop(listen_dsn, drain_tx, listen_shutdown).await;
                }));

                // Outbox cleanup loop
                let cleanup_pool = pool.clone();
                let cleanup_shutdown = shutdown_rx.clone();
                outbox_cleanup_handle = Some(tokio::spawn(async move {
                    outbox::run_outbox_cleanup(cleanup_pool, cleanup_shutdown).await;
                }));
            } else if telegram_only {
                // Telegram-only mode: the poller writes directly to Postgres
                // and calls enqueue_message_check(). No outbox or polling needed.
                info!("Telegram-only mode — outbox and message_loop disabled (poller handles ingress)");
            } else {
                // Legacy polling mode
                info!("legacy polling mode — spawning message_loop");

                let ml_config = message_loop::MessageLoopConfig {
                    poll_interval_ms: state.config.orchestrator.poll_interval_ms,
                    assistant_name: assistant_name.clone(),
                    main_group_folder: state.config.orchestrator.main_group_folder.clone(),
                };
                let ml_pool = pool.clone();
                let ml_queue = state.queue.clone();
                let ml_groups = state.groups.clone();
                let ml_timestamps = state.agent_timestamps.clone();
                let ml_shutdown = shutdown_rx.clone();
                message_loop_handle = Some(tokio::spawn(async move {
                    message_loop::run_message_loop(
                        ml_config, ml_pool, ml_queue, ml_groups, ml_timestamps, ml_shutdown,
                    )
                    .await;
                }));
            }

            // Scheduler loop (runs in both modes)
            let sched_config = scheduler::SchedulerConfig {
                poll_interval: std::time::Duration::from_millis(
                    state.config.scheduler.poll_interval_ms,
                ),
                timezone: state.config.scheduler.timezone.clone(),
                enabled: state.config.scheduler.enabled,
                batch_size: 10,
            };
            let task_callback = scheduler_wiring::build_task_callback(
                pool.clone(),
                state.queue.clone(),
                state.groups.clone(),
                state.sessions.clone(),
                state.telegram.clone(),
                run_config,
                state.config.scheduler.timezone.clone(),
            );
            let sched_pool = pool.clone();
            let sched_shutdown = shutdown_rx.clone();
            scheduler_handle = Some(tokio::spawn(async move {
                scheduler::run_scheduler_loop(
                    sched_config, sched_pool, task_callback, sched_shutdown,
                )
                .await;
            }));

            info!("orchestrator enabled: scheduler wired");
        } else {
            tracing::warn!("orchestrator.enabled=true but no Postgres connection — orchestrator disabled");
        }
    }

    // Telegram update poller — replaces Node/grammY for inbound messages
    let mut telegram_poller_handle: Option<tokio::task::JoinHandle<()>> = None;
    if state.telegram.is_enabled() {
        if let Some(ref pool) = state.db {
            let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
                .unwrap_or_default()
                .trim()
                .to_string();
            if !bot_token.is_empty() {
                let poller_config = telegram_poller::TelegramPollerConfig {
                    poll_timeout_secs: 30,
                    groups_dir: project_root.join("groups"),
                    assistant_name: std::env::var("ASSISTANT_NAME")
                        .unwrap_or_else(|_| "Amtiskaw".into()),
                    main_group_folder: state.config.orchestrator.main_group_folder.clone(),
                    started_at: state.started_at,
                };
                let poller = telegram_poller::TelegramPoller::new(
                    poller_config,
                    bot_token,
                    state.telegram.clone(),
                    state.demarch.clone(),
                    state.config.clone(),
                    pool.clone(),
                    state.queue.clone(),
                    state.groups.clone(),
                    state.sessions.clone(),
                );
                let poller_shutdown = shutdown_rx.clone();
                telegram_poller_handle = Some(tokio::spawn(async move {
                    poller.run(poller_shutdown).await;
                }));
                info!("Telegram poller enabled — receiving updates directly in Rust");
            }
        } else {
            warn!("Telegram bridge enabled but no Postgres — poller disabled");
        }
    }

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
        .route("/v1/telegram/callback", post(telegram_callback))
        .route("/v1/commands", post(handle_slash_command))
        // groups/set on the main router so it can update the in-memory map
        .route("/v1/db/groups/set-sync", post(set_registered_group_sync))
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
    if let Some(h) = registry_handle {
        let _ = h.await;
    }
    let _ = events_handle.await;
    if let Some(h) = message_loop_handle {
        let _ = h.await;
    }
    if let Some(h) = outbox_drain_handle {
        let _ = h.await;
    }
    if let Some(h) = outbox_listen_handle {
        let _ = h.await;
    }
    if let Some(h) = outbox_cleanup_handle {
        let _ = h.await;
    }
    if let Some(h) = scheduler_handle {
        let _ = h.await;
    }
    if let Some(h) = telegram_poller_handle {
        let _ = h.await;
    }

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
    let groups_count = state.groups.read().await.len();
    let active = state.queue.active_count().await;
    let (outbox_pending, outbox_failed) = if state.config.orchestrator.use_outbox {
        if let Some(ref pool) = state.db {
            match pool.outbox_stats().await {
                Ok(stats) => (Some(stats.pending), Some(stats.failed)),
                Err(_) => (None, None),
            }
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };
    Json(ReadyResponse {
        status: "ready",
        runtime_profiles: state.config.runtimes.profiles.len(),
        demarch_writes_restricted_to_main: state.config.demarch.require_main_group_for_writes,
        telegram_bridge_enabled: state.telegram.is_enabled(),
        postgres_connected: state.db.is_some(),
        orchestrator_enabled: state.config.orchestrator.enabled,
        use_outbox: state.config.orchestrator.use_outbox,
        registered_groups: groups_count,
        active_containers: active,
        outbox_pending,
        outbox_failed,
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

async fn telegram_callback(
    State(state): State<AppState>,
    Json(request): Json<TelegramCallbackRequest>,
) -> Json<TelegramCallbackResponse> {
    match state.telegram.handle_callback(request, &state.demarch, &state.config).await {
        Ok(response) => Json(response),
        Err(err) => Json(TelegramCallbackResponse {
            ok: false,
            action: String::new(),
            target_id: String::new(),
            result: None,
            error: Some(err.to_string()),
        }),
    }
}

/// Fetch Intercore run info for /status enrichment.
/// Degrades gracefully: returns None on any error.
pub async fn fetch_run_info(demarch: &DemarchAdapter) -> Option<commands::RunInfo> {
    // 1. Get current run (lightweight: { found, id, goal, phase })
    let current_resp = demarch.execute_read(ReadOperation::RunStatus { run_id: None });
    if current_resp.status != intercom_core::DemarchStatus::Ok {
        return None;
    }
    let current: serde_json::Value = serde_json::from_str(&current_resp.result).ok()?;
    if !current.get("found")?.as_bool()? {
        return None;
    }
    let run_id = current.get("id")?.as_str()?.to_string();
    let goal = current.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let phase = current.get("phase").and_then(|v| v.as_str()).unwrap_or("").to_string();

    // 2. Get full run details (phases array, token_budget)
    let full_resp = demarch.execute_read(ReadOperation::RunStatus { run_id: Some(run_id.clone()) });
    let (phase_index, phase_count, token_budget) = if full_resp.status == intercom_core::DemarchStatus::Ok {
        let full: serde_json::Value = serde_json::from_str(&full_resp.result).unwrap_or_default();
        let phases = full.get("phases").and_then(|v| v.as_array());
        let pi = phases.as_ref().and_then(|ps| ps.iter().position(|p| p.as_str() == Some(&phase))).unwrap_or(0);
        let pc = phases.map(|ps| ps.len()).unwrap_or(0);
        let tb = full.get("token_budget").and_then(|v| v.as_u64()).unwrap_or(0);
        (pi, pc, tb)
    } else {
        (0, 0, 0)
    };

    // 3. Get token spend
    let tokens_resp = demarch.execute_read(ReadOperation::RunTokens { run_id: run_id.clone() });
    let tokens_spent = if tokens_resp.status == intercom_core::DemarchStatus::Ok {
        let tokens: serde_json::Value = serde_json::from_str(&tokens_resp.result).unwrap_or_default();
        tokens.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
    } else {
        0
    };

    // 4. Get active dispatch count
    let dispatch_resp = demarch.execute_read(ReadOperation::DispatchList { active_only: true });
    let active_dispatches = if dispatch_resp.status == intercom_core::DemarchStatus::Ok {
        let dispatches: serde_json::Value = serde_json::from_str(&dispatch_resp.result).unwrap_or_default();
        dispatches.as_array().map(|a| a.len()).unwrap_or(0)
    } else {
        0
    };

    Some(commands::RunInfo {
        run_id,
        goal,
        phase,
        phase_index,
        phase_count,
        token_budget,
        tokens_spent,
        active_dispatches,
    })
}

/// Write a group to Postgres AND update the in-memory groups map.
/// Use this instead of /v1/db/groups/set when the caller needs the Rust
/// orchestrator to immediately see the change.
async fn set_registered_group_sync(
    State(state): State<AppState>,
    Json(group): Json<RegisteredGroup>,
) -> impl IntoResponse {
    let pool = match &state.db {
        Some(p) => p,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "postgres not available"})),
            )
                .into_response()
        }
    };
    if let Err(e) = pool.set_registered_group(&group).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // Update in-memory map so the orchestrator sees the group immediately
    {
        let mut groups = state.groups.write().await;
        groups.insert(group.jid.clone(), group);
    }
    (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response()
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
    // Fetch Intercore run info for /status enrichment
    let run_info = if request.command == "status" {
        fetch_run_info(&state.demarch).await
    } else {
        None
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
        run_info.as_ref(),
    );

    // Apply side effects
    if !result.effects.is_empty() {
        apply_command_effects(
            &state,
            &request.chat_jid,
            request.group_folder.as_deref(),
            &result.effects,
        )
        .await;
    }

    Json(result)
}

/// Apply side effects from command handlers.
async fn apply_command_effects(
    state: &AppState,
    chat_jid: &str,
    group_folder: Option<&str>,
    effects: &[commands::CommandEffect],
) {
    for effect in effects {
        match effect {
            commands::CommandEffect::KillContainer => {
                state.queue.kill_group(chat_jid).await;
            }
            commands::CommandEffect::ClearSession => {
                if let Some(folder) = group_folder {
                    // Clear in-memory
                    state.sessions.write().await.remove(folder);
                    // Clear in Postgres
                    if let Some(ref pool) = state.db {
                        if let Err(e) = pool.delete_session(folder).await {
                            tracing::warn!(err = %e, folder, "failed to delete session");
                        }
                    }
                }
            }
            commands::CommandEffect::SwitchModel {
                model_id,
                runtime,
            } => {
                if let Some(folder) = group_folder {
                    // Update in-memory group
                    let mut groups = state.groups.write().await;
                    if let Some(group) = groups.values_mut().find(|g| g.folder == folder) {
                        group.model = Some(model_id.clone());
                        group.runtime = Some(runtime.clone());

                        // Persist to Postgres
                        if let Some(ref pool) = state.db {
                            if let Err(e) = pool.set_registered_group(group).await {
                                tracing::warn!(err = %e, folder, "failed to persist model switch");
                            }
                        }
                    }
                }
            }
        }
    }
}
