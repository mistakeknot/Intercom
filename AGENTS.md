# Intercom — Developer & Agent Guide

Multi-runtime personal AI assistant with container isolation and Telegram integration. Single-binary Rust daemon (`intercomd`) handles the full message lifecycle: Telegram update polling, message routing, Postgres persistence, container dispatch, scheduling, and Telegram delivery. See [`PHILOSOPHY.md`](../../PHILOSOPHY.md) for design direction.

Node host (`src/`) is archived — kept for potential WhatsApp (Baileys) revival but not running in production.

## Architecture Overview

```
Telegram API
        |
        v
   intercomd (Rust — single binary)
   ├── telegram_poller.rs   (getUpdates long-polling, message/media/command parsing)
   ├── telegram.rs           (outbound Telegram API: send, edit, buttons, callbacks)
   ├── ipc.rs                (IPC watcher + TelegramDelegate)
   ├── events.rs             (kernel event consumer)
   ├── commands.rs           (slash command handler)
   ├── process_group.rs      (container orchestrator)
   ├── container/runner.rs   (async container spawning)
   ├── scheduler_wiring.rs   (task scheduler)
   ├── db.rs                 (Postgres persistence)
   └── main.rs               (Axum HTTP server + background loops)
        |
        v
   Docker Container (one per active conversation)
   ├── Claude runtime     → intercom-agent:latest
   ├── Gemini runtime     → intercom-agent-gemini:latest
   └── Codex runtime      → intercom-agent-codex:latest
```

**Telegram-Only Mode**: Auto-detected when `TELEGRAM_BOT_TOKEN` is set and `TelegramBridge` is enabled. The poller writes directly to Postgres and enqueues messages for processing — no outbox indirection needed. `TelegramDelegate` in `ipc.rs` sends messages directly via `TelegramBridge` (no HTTP to Node). Registry sync, outbox drain, and message_loop are all skipped.

**Container Output**: Containers prefer Unix domain sockets (UDS) for output when available, falling back to stdout `OUTPUT_START/END` markers for backward compatibility. The host binds a `UnixListener` at `{data_dir}/ipc/{group}/output.sock` before spawning each container. The container connects and sends length-prefixed frames (4-byte big-endian length + JSON payload, max 4 MiB). UDS eliminates marker-splitting bugs from partial reads, embedded newlines, and large output truncation. Protocol implementation: `container/shared/protocol.ts` (client), `rust/intercomd/src/container/runner.rs` (Rust server), `go/internal/container/runner.go` (Go server).

## Architecture Reference

| Topic | Doc |
|-------|-----|
| Message flow, Telegram polling, direct Postgres write | [Message Flow](docs/architecture/message-flow.md) |
| Runtime selection, container images, protocol, IPC | [Multi-Runtime System](docs/architecture/runtimes.md) |
| Crate structure, config, CLI, HTTP API, background loops | [IronClaw (Rust Daemon)](docs/architecture/ironclaw.md) |
| Host/Rust/Container file tables | [File Reference](docs/architecture/file-reference.md) |
| Container isolation, secrets, mount validation, allowlists | [Security Model](docs/architecture/security.md) |

## Go Rewrite (in progress)

The daemon is being rewritten from Rust to Go (`go/`). Motivation: Demarch's stack is overwhelmingly Go — Skaffen, Clavain, Intercore, masaq. In Go, cross-pillar integrations become direct function calls instead of subprocess bridges.

### Go Package Structure

```
go/
├── cmd/intercomd/main.go          # CLI + Axum-equivalent HTTP server
└── internal/
    ├── config/                    # TOML config with env overrides
    ├── container/                 # Docker container execution (NEW)
    │   ├── protocol.go            #   ContainerInput/Output types, OUTPUT markers, RuntimeKind
    │   ├── mounts.go              #   Volume mount builder, GroupInfo, ContainerName()
    │   ├── secrets.go             #   .env parsing, Claude OAuth, Docker arg construction
    │   ├── security.go            #   MountAllowlist, blocked patterns, path validation
    │   └── runner.go              #   RunContainerAgent(), UDS streaming, timeout watchdog
    ├── db/                        # Postgres (pgx): groups, messages, outbox, LISTEN/NOTIFY
    ├── mcp/                       # MCP server for custom tools
    ├── outbox/                    # Outbox drain loop with LISTEN/NOTIFY
    ├── queue/                     # Message queue + delivery
    ├── routing/                   # Skaffen router integration
    ├── scheduler/                 # Cron/interval/one-shot scheduling
    ├── smoke/                     # End-to-end smoke tests
    ├── subprocess/                # CLI agent subprocess manager
    │   ├── process.go             #   Process lifecycle, result timeout watchdog
    │   ├── manager.go             #   Concurrency limits, session tracking
    │   ├── session.go             #   Per-group session files, auto-reset
    │   └── mcpconfig.go           #   Auto-generated MCP config
    └── telegram/                  # Telegram bot (poller, commands, delivery)
```

### Container Package (`go/internal/container/`)

Full port of the Rust `container/` module. Manages Docker container lifecycle for agent runtimes.

| File | Purpose |
|------|---------|
| `protocol.go` | Wire types: `ContainerInput`, `ContainerOutput`, `StreamEvent`, `VolumeMount`. `ExtractOutputMarkers()` for stdout fallback parsing. `ContainerImage()` / `RunnerContainerPath()` for runtime→image mapping. |
| `mounts.go` | `BuildVolumeMounts()` — constructs bind mounts per group. Main groups get project root (ro) + group dir (rw). Claude runtime gets `.claude/` sessions with auto-created settings. All groups get IPC namespace and runner source mounts. |
| `secrets.go` | `ReadSecrets()` — reads `.env` + Claude OAuth credentials. `BuildContainerArgs()` / `BuildPoolContainerArgs()` — constructs `docker run` CLI with `-v`, `--user`, `--mount tmpfs` for excluded dirs. |
| `security.go` | `MountAllowlist` from `~/.config/intercom/mount-allowlist.json`. `ValidateMount()` checks: hard-blocked roots, blocked patterns (`.ssh`, `.env`, etc.), allowed root containment, path traversal, read-only enforcement for non-main groups. |
| `runner.go` | `RunContainerAgent()` — spawns Docker, writes JSON to stdin, streams output via UDS (length-prefixed frames) or stdout markers. Two-phase timeout watchdog: startup (30s) then activity-based. `EnsureRuntimeAvailable()`, `CleanupOrphans()`, `StopContainer()`. |

**Integration**: `subprocess.StartConfig` has a `UseContainer bool` field. When true, callers route through `container.RunContainerAgent()` instead of `subprocess.Start()`.

### Go Build & Test

```bash
cd go && go build ./...             # Build all packages
cd go && go test ./...              # Run all tests (34 container + others)
cd go && go test ./internal/container/ -v  # Container package tests only
```

## Service Management

Single systemd user service:

```bash
systemctl --user {start|stop|restart|status} intercomd
journalctl --user -u intercomd -f
```

## Development

### Build & Run

```bash
npm run rust:check                        # Check Rust workspace
npm run rust:build                        # Build Rust workspace (debug)
npm run rust:build:release                # Build Rust workspace (release)
npm run rust:test                         # Run Rust tests (161+ tests)
cd container && bash build.sh latest all  # Build all container images
cd container && bash build.sh latest gemini  # Build single runtime
```

**Always restart intercomd after building.** The Rust binary is only loaded at process startup.

### Hot Reload

Runner source and shared code are bind-mounted from host into all containers (`/app/{runner}/src` and `/app/shared`) and recompiled on startup. Edit `container/*/src/*.ts` or `container/shared/*.ts` — changes take effect on next container spawn without rebuilding Docker images.

### Container Rebuild Rule

Rebuild container images after modifying runner source that changes dependencies or structure. Runner code changes (logic only) are picked up via hot reload.

```bash
cd container && bash build.sh latest <runtime>  # claude, gemini, codex, or all
```

### Postgres

Rust orchestration requires Postgres (`orchestrator.enabled=true` + `postgres_dsn` in `intercom.toml`). Postgres runs as a Docker container (`intercom-postgres`, port 5432, `--restart unless-stopped`, volume `intercom-pgdata`). Schema auto-created by `ensure_schema()` at startup.

## Gotchas

- **Container rebuild**: `--no-cache` doesn't invalidate COPY steps. Prune buildkit to force clean rebuild.
- **Hot reload**: Source mounted read-only and recompiled inside the container. Edit host files, not container files.
- **Gemini OAuth scope**: The Gemini CLI token has `cloud-platform` scope, not `generative-language`. Must use Code Assist API.
- **Codex auth.json format**: Rust parser is strict — needs all four fields: `id_token`, `access_token`, `refresh_token`, `account_id`.
- **Gemini thinking tokens**: Thinking parts (`thought: true`) count against maxOutputTokens and must be filtered from output.
- **Orchestrator flag**: `orchestrator.enabled` is true in production. Requires Postgres connection (`intercom-postgres` Docker container). Logs a warning and disables itself if Postgres is unavailable.
- **Build then restart**: `cargo build` produces the binary loaded only at process startup. Always `systemctl --user restart intercomd` after building.
- **--pull=never on container spawn**: `secrets.rs` passes `--pull=never` to Docker. Container images must be pre-built locally (`bash build.sh latest all`). Docker will NOT pull from a registry — missing images cause immediate failure.
- **Container startup timeout**: A 30-second watchdog kills containers that produce no output (via UDS or stdout). If a container image is broken or hangs during init, it fast-fails instead of blocking the group queue indefinitely. The watchdog transitions to activity-based timeout after first output.
- **UDS fallback**: If the UDS socket doesn't exist (old host) or the container can't connect (timeout 2s), it silently falls back to stdout markers. Look for `UDS output connected` in container stderr or `UDS connection accepted` in intercomd logs to confirm UDS is active.
- **Queue drain after container exit**: `queue.rs` drains pending messages/tasks after every container completion via `drain_pending()`. If you modify `reset_group()` or the run loop, ensure drain still runs — without it, messages arriving during an active container are silently dropped after the container exits.

<!-- Session close protocol inherited from root AGENTS.md. -->
