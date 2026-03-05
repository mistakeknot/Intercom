# Intercom — Developer & Agent Guide

Multi-runtime personal AI assistant with container isolation and messaging integration. Dual-process architecture: **Node host** (channels, commands, host callback) and **IronClaw** (Rust daemon `intercomd`, orchestration, container dispatch, scheduling, Postgres persistence). See [`PHILOSOPHY.md`](../../PHILOSOPHY.md) for design direction.

## Architecture Overview

```
Telegram / WhatsApp
        |
        v
   Node Host (channel layer)               Rust Daemon (IronClaw)
   ├── channels/telegram.ts                ├── telegram.rs          (Telegram bridge API)
   ├── channels/whatsapp.ts                ├── ipc.rs               (IPC watcher + delegation)
   ├── index.ts (channels + commands)      ├── events.rs            (kernel event consumer)
   ├── container-runner.ts                 ├── commands.rs          (slash command handler)
   ├── host-callback.ts ◄─── HTTP ───────►├── process_group.rs     (container orchestrator)
   ├── intercomd-client.ts ──── HTTP ─────►├── container/runner.rs  (async container spawning)
   ├── query-handlers.ts                   ├── scheduler_wiring.rs  (task scheduler)
   ├── db.ts (SQLite)                      ├── db.rs                (Postgres persistence)
   └── ipc.ts                              └── main.rs              (Axum HTTP server)
        |                                          |
        v                                          v
   Docker Container (one per active conversation)
   ├── Claude runtime     → intercom-agent:latest
   ├── Gemini runtime     → intercom-agent-gemini:latest
   └── Codex runtime      → intercom-agent-codex:latest
```

**IronClaw**: intercomd is the orchestrator — handles the full message loop, container spawning, scheduling, and Telegram bridge natively in Rust. Node serves as the channel layer: receives messages from WhatsApp/Telegram, routes commands, and delegates container spawning back to intercomd via HTTP callbacks. The `orchestrator.enabled` flag in `intercom.toml` controls whether Rust runs the message loop (requires Postgres; sidecar mode when disabled).

**Container Output**: Containers prefer Unix domain sockets (UDS) for output when available, falling back to stdout `OUTPUT_START/END` markers for backward compatibility. The host binds a `UnixListener` at `{data_dir}/ipc/{group}/output.sock` before spawning each container. The container connects and sends length-prefixed frames (4-byte big-endian length + JSON payload, max 4 MiB). UDS eliminates marker-splitting bugs from partial reads, embedded newlines, and large output truncation. Protocol implementation: `container/shared/protocol.ts` (client), `rust/intercomd/src/container/runner.rs` (server).

**Outbox**: When `orchestrator.use_outbox=true`, message delivery uses a durable write path instead of polling. Node writes to the `message_outbox` Postgres table (via `pg-writer.ts`), a Postgres trigger fires `NOTIFY intercom_outbox`, and the Rust LISTEN loop (`outbox.rs`) signals the drain loop to claim rows, store messages, and enqueue processing. This replaces the legacy polling-based message loop.

## Architecture Reference

| Topic | Doc |
|-------|-----|
| Outbox pipeline, row lifecycle, payload types, legacy mode | [Message Flow](docs/architecture/message-flow.md) |
| Runtime selection, container images, protocol, IPC | [Multi-Runtime System](docs/architecture/runtimes.md) |
| Crate structure, config, CLI, HTTP API, background loops | [IronClaw (Rust Daemon)](docs/architecture/ironclaw.md) |
| Host/Rust/Container file tables | [File Reference](docs/architecture/file-reference.md) |
| Container isolation, secrets, mount validation, allowlists | [Security Model](docs/architecture/security.md) |

## Service Management

Two systemd user services run together:

```bash
# Node host (NanoClaw)
systemctl --user start intercom
systemctl --user stop intercom
systemctl --user restart intercom
journalctl --user -u intercom -f

# Rust daemon (IronClaw)
systemctl --user start intercomd
systemctl --user stop intercomd
systemctl --user restart intercomd
journalctl --user -u intercomd -f
```

`intercomd.service` is configured to start before `intercom.service` so IPC queries are handled from boot.

## Development

### Build & Run

```bash
npm run dev                               # Node host with hot reload
npm run build                             # Compile TypeScript
npm run rust:check                        # Check Rust workspace
npm run rust:build                        # Build Rust workspace (debug)
npm run rust:build:release                # Build Rust workspace (release)
npm run rust:test                         # Run Rust tests (161 tests)
npm test                                  # Run vitest (Node tests)
npm run typecheck                         # TypeScript type checking
cd container && bash build.sh latest all  # Build all container images
cd container && bash build.sh latest gemini  # Build single runtime
```

**Always restart services after building.** Compiled JS in `dist/` and the Rust binary are only loaded at process startup.

### Hot Reload

Runner source and shared code are bind-mounted from host into all containers (`/app/{runner}/src` and `/app/shared`) and recompiled on startup. Edit `container/*/src/*.ts` or `container/shared/*.ts` — changes take effect on next container spawn without rebuilding Docker images.

### Container Rebuild Rule

Rebuild container images after modifying runner source that changes dependencies or structure. Runner code changes (logic only) are picked up via hot reload.

```bash
cd container && bash build.sh latest <runtime>  # claude, gemini, codex, or all
```

### Rust-to-Node Wiring

Rust orchestration requires Postgres (`orchestrator.enabled=true` + `postgres_dsn` in `intercom.toml`). Postgres runs as a Docker container (`intercom-postgres`, port 5432, `--restart unless-stopped`, volume `intercom-pgdata`). Without Postgres, intercomd runs as a sidecar (Telegram bridge, IPC, events) and Node handles orchestration. Node routes Telegram ingress/egress through intercomd unconditionally with automatic fallback if unavailable. The host callback server starts on `HOST_CALLBACK_PORT` (default 7341).

## Gotchas

- **Container rebuild**: `--no-cache` doesn't invalidate COPY steps. Prune buildkit to force clean rebuild.
- **Hot reload**: Source mounted read-only and recompiled inside the container. Edit host files, not container files.
- **Gemini OAuth scope**: The Gemini CLI token has `cloud-platform` scope, not `generative-language`. Must use Code Assist API.
- **Codex auth.json format**: Rust parser is strict — needs all four fields: `id_token`, `access_token`, `refresh_token`, `account_id`.
- **Gemini thinking tokens**: Thinking parts (`thought: true`) count against maxOutputTokens and must be filtered from output.
- **Service restart order**: intercomd must start before intercom (configured via systemd `Before=` directive).
- **Orchestrator flag**: `orchestrator.enabled` is true in production. Requires Postgres connection (`intercom-postgres` Docker container). Logs a warning and disables itself if Postgres is unavailable.
- **Build then restart**: Both `npm run build` and `cargo build` produce artifacts loaded only at process startup. Always restart the corresponding service after building.
- **INTERCOM_POSTGRES_DSN required for outbox**: Node's `pg-writer.ts` needs `INTERCOM_POSTGRES_DSN` in `.env` to write to the outbox. Without it, messages fall back to HTTP POST (`/v1/db/messages`) which stores but does NOT trigger processing in outbox mode. Messages silently accumulate without being dispatched.
- **cleanupOrphans exclusion**: `container-runtime.ts` has a `CLEANUP_EXCLUDE` set containing `intercom-postgres`. Without this, the orphan cleanup kills the Postgres infrastructure container on every Node restart.
- **--pull=never on container spawn**: `secrets.rs` passes `--pull=never` to Docker. Container images must be pre-built locally (`bash build.sh latest all`). Docker will NOT pull from a registry — missing images cause immediate failure.
- **Container startup timeout**: A 30-second watchdog kills containers that produce no output (via UDS or stdout). If a container image is broken or hangs during init, it fast-fails instead of blocking the group queue indefinitely. The watchdog transitions to activity-based timeout after first output.
- **UDS fallback**: If the UDS socket doesn't exist (old host) or the container can't connect (timeout 2s), it silently falls back to stdout markers. Look for `UDS output connected` in container stderr or `UDS connection accepted` in intercomd logs to confirm UDS is active.
- **Outbox stale recovery**: Rows stuck in `processing` state (from crashes) are recovered at startup and every 5 minutes. If you see duplicate message processing after a crash, this is the recovery mechanism working as intended.
- **Dual-database state**: Node reads groups from SQLite (`store/messages.db`), Rust reads from Postgres. Both have `registered_groups` tables. When modifying group config (e.g. `container_config`), update **both** databases or the change is invisible to one side. Postgres is the production truth when the orchestrator is enabled.
- **Queue drain after container exit**: `queue.rs` drains pending messages/tasks after every container completion via `drain_pending()`. If you modify `reset_group()` or the run loop, ensure drain still runs — without it, messages arriving during an active container are silently dropped after the container exits.

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd sync
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
