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
    ├── telegram/                  # Telegram bot (poller, commands, delivery) [migrating to transport/telegram/]
    └── transport/                 # Canonical message-passing surface (A2A-shaped)
        └── transport.go           #   Transport interface + InboundMessage/OutboundMessage/Part types
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

### Transport Package (`go/internal/transport/`)

Canonical message-passing surface. Anchors on the A2A protocol per [`docs/canon/intercom-transport-target.md`](../../docs/canon/intercom-transport-target.md) — A2A is the native internal form; telegram and signal are wire adapters that translate to/from A2A `Message` / `Task` shapes.

| File | Purpose |
|------|---------|
| `transport.go` | The `Transport` interface (`Name`, `Send`, `Subscribe`, `Health`) plus canonical types: `InboundMessage`, `OutboundMessage`, `Part` (text/file/data), `PartKind`, `Health`. |
| `transport_test.go` | Compile-time interface assertion via `mockTransport`; PartKind string-table tests; zero-value safety tests. |
| `a2a/types.go` | A2A wire types: `Message`, `Part` (text/file/data), `SendMessageRequest`/`Response`, `ListTasksResponse`, `Task`/`TaskStatus`/`TaskState`, `Artifact`, `AgentCard`, `AgentCapabilities`, `AgentSkill`, `SecurityScheme`, `OAuth2Flow`. |
| `a2a/translate.go` | A2A `Message` ↔ canonical `InboundMessage`/`OutboundMessage`. Recognizes `Metadata["sylveste.senderUri"]`; passes through metadata as `WireMetadata["a2a.meta.*"]`. |
| `a2a/store.go` | `Store` — in-memory Task store with cursor pagination, terminal-state guard, RW-lock concurrency. v2 lands a Dolt-backed variant behind the same interface. |
| `a2a/server.go` | `Server` (implements `Transport`): HTTP handlers using Go 1.22+ method-aware patterns. Routes: `GET /.well-known/agent.json`, `POST /messages` (creates+stores Task), `POST /messages:stream` (SSE), `GET /tasks` (List w/ pagination), `GET /tasks/{id}` (Get; `:subscribe` suffix→SSE), `POST /tasks/{id}:cancel` (Cancel). Backpressure via blocking channel send under request context. Outbound `Send` resolves recipient → POSTs `/messages` (see `outbound.go`). |
| `a2a/outbound.go` | Outbound A2A client. `Resolver` interface (with `MapResolver` for tests/static deployments) maps `sylveste://agent/<name>` → base HTTP URL. `WithBearerToken` injects an OAuth2 token onto outbound ctx; surfaces on the wire as `Authorization: Bearer <token>` (full acquisition lives in `.4`). Agent Card cache with TTL (`DefaultAgentCardTTL=5m`) avoids per-send `/.well-known/agent.json` fetches. |
| `a2a/broker.go` | `Broker` fans out per-task SSE events. Buffered subscriber channels, non-blocking `Publish` (best-effort), `PublishFinal` closes all subscribers + marks task terminated so late subscribers get a closed channel. |
| `a2a/stream.go` | SSE handlers for `POST /messages:stream` and `GET /tasks/{id}:subscribe`. Wire format `event: <kind>\ndata: <json>\n\n`. Backfills current status on subscribe; emits terminal-state event with `Final: true`. |
| `a2a/server_test.go` | Full lifecycle round-trip: POST /messages → list → get → cancel → re-cancel-409 → subscribe (200, text/event-stream). Plus Agent Card, missing-messageId, invalid-limit, transport.Transport assertion. |
| `a2a/store_test.go` | Store unit tests: create+get, state transitions, terminal-state guard, cancel (active + terminal + unknown), cursor pagination across multiple pages, limit clamping, empty/unknown cursor, concurrent Len. Plus `splitTaskIDSuffix` parser. |
| `a2a/stream_test.go` | SSE behavior: stream emits SUBMITTED→WORKING→COMPLETED on POST /messages:stream; backfill-then-stream on /tasks/{id}:subscribe; terminal-task backfill emits final-and-closes; client cancel cleans up broker subscriber. Plus broker unit tests (subscribe/unsubscribe, PublishFinal closes, slow-subscriber drop). |
| `a2a/outbound_test.go` | Outbound behavior: caller↔peer round-trip via two co-hosted Servers; ErrNoResolver / wrapped ErrUnknownRecipient sentinels; bearer-token pass-through (and absence); peer non-2xx surfaces error; card cache hits after first fetch; TTL expiry. |
| `a2a/idgen.go` | Monotonic ID generators: `generateMessageID` (`a2a-<nanos>-<n>`) and `generateTaskID` (`task-<nanos>-<n>`). Distinct prefixes keep grep/diagnosis trivial. |

**Implementation status (2026-05-24):**
- Interface defined and tested. `go test ./internal/transport/...` passes under `-race`.
- A2A native transport: v1 inbound + Agent Card landed under `sylveste-ewy3.4.1`.
- A2A Task store + GET/POST `/tasks` endpoints landed under `sylveste-ewy3.4.1.2`.
- A2A SSE streaming (POST /messages:stream, GET /tasks/{id}:subscribe) landed under `sylveste-ewy3.4.1.1`. `AgentCard.Capabilities.Streaming` is auto-set to true by `New()`.
- A2A outbound client (Server.Send → resolver → /messages POST) landed under `sylveste-ewy3.4.1.3`. `ErrOutboundNotImplemented` is gone; the wire is symmetric.
- Remaining sub-bead under `sylveste-ewy3.4.1`:
  - `.4` OAuth2 Resource Indicators enforcement (depends on Gridfire-v1)
- Concrete implementations for adjacent wires land via separate beads:
  - `transport/telegram/` — migrate from `internal/telegram/` (bead `sylveste-benl.7`)
  - `transport/signal/` — new (bead `sylveste-benl.6`)

**Contract for implementers:**

1. **Inbound translation:** wire-specific events become `InboundMessage` with `TransportName` set, `SenderURI` in canonical scheme (`telegram:<user_id>`, `signal:<e164>`, `sylveste://agent/<name>`), `Parts` filled from media type (text/file/data), `WireMetadata` carrying transport-specific round-trip fields (e.g. telegram `chat_id` / `message_id`). The routing layer never reads `WireMetadata` — it passes through opaquely.
2. **Outbound translation:** consume `OutboundMessage`, write to wire. Recognize hint keys in `WireMetadata` (e.g. `reply_to_msg_id`); ignore unknown keys without error.
3. **Backpressure:** do not buffer more than ~100 messages on the `Subscribe` channel. Deliver backpressure to the wire (pause polling, etc.).
4. **Concurrency:** `Send` MUST be safe for concurrent calls. `Subscribe` is called once per process lifecycle.
5. **Health:** must be cheap; the `/health` HTTP endpoint and SessionStart hooks call it frequently.

**Sprint↔Task adapter** (per intercom-transport-target.md §Sylveste-sprint↔A2A-Task adapter):
- `InboundMessage.ContextID` / `OutboundMessage.ContextID` = bead ID (durable conversation thread).
- `OutboundMessage.TaskID` = A2A runtime task handle (per-run, ephemeral).
- The scheduler keys work by both: runtime by TaskID, durable by ContextID.

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
