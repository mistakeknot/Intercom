use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use intercom_compat::{LegacyLayout, LegacySnapshot, inspect_legacy_layout, inspect_legacy_sqlite};
use intercom_core::{
    DemarchAdapter, DemarchResponse, IntercomConfig, ReadOperation, WriteOperation, load_config,
};
use serde::{Deserialize, Serialize};
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

#[derive(Clone)]
struct AppState {
    started_at: Instant,
    config: Arc<IntercomConfig>,
    demarch: Arc<DemarchAdapter>,
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
    let project_root =
        std::env::current_dir().context("failed to resolve current working directory")?;
    let demarch = DemarchAdapter::new(config.demarch.clone(), &project_root);
    let state = AppState {
        started_at: Instant::now(),
        config: Arc::new(config),
        demarch: Arc::new(demarch),
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/runtime/profiles", get(runtime_profiles))
        .route("/v1/demarch/read", post(demarch_read))
        .route("/v1/demarch/write", post(demarch_write))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("failed to bind listener on {bind}"))?;

    info!(bind = %bind, "intercomd listening");
    axum::serve(listener, app)
        .await
        .context("server exited unexpectedly")
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
