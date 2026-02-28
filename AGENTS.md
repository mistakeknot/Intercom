# Intercom — Developer & Agent Guide

## Canonical References
1. [`PHILOSOPHY.md`](../../PHILOSOPHY.md) — direction for ideation and planning decisions.
2. `CLAUDE.md` — implementation details, architecture, testing, and release workflow.

## Philosophy Alignment Protocol
Review [`PHILOSOPHY.md`](../../PHILOSOPHY.md) during:
- Intake/scoping
- Brainstorming
- Planning
- Execution kickoff
- Review/gates
- Handoff/retrospective

For brainstorming/planning outputs, add two short lines:
- **Alignment:** one sentence on how the proposal supports the module's purpose within Demarch's philosophy.
- **Conflict/Risk:** one sentence on any tension with philosophy (or 'none').

If a high-value change conflicts with philosophy, either:
- adjust the plan to align, or
- create follow-up work to update `PHILOSOPHY.md` explicitly.


Multi-runtime personal AI assistant with container isolation and messaging integration. Dual-process architecture: **Node host** (channels, commands, host callback) and **IronClaw** (Rust daemon `intercomd`, orchestration, container dispatch, scheduling, Postgres persistence).

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

**IronClaw architecture**: intercomd is the orchestrator — handles the full message loop, container spawning, scheduling, and Telegram bridge natively in Rust. Node serves as the channel layer: receives messages from WhatsApp/Telegram, routes commands, and delegates container spawning back to intercomd via HTTP callbacks. The `orchestrator.enabled` flag in `intercom.toml` controls whether Rust runs the message loop (default: `true`).

## Multi-Runtime System

### Runtime Selection

Default runtime set via `INTERCOM_RUNTIME` env var (values: `claude`, `gemini`, `codex`). Per-group override via `runtime` field on `RegisteredGroup`. Resolution: `group.runtime || DEFAULT_RUNTIME`.

### Container Images

| Runtime | Image | Backend | Auth |
|---------|-------|---------|------|
| claude | `intercom-agent:latest` | Claude Agent SDK | `CLAUDE_CODE_OAUTH_TOKEN` |
| gemini | `intercom-agent-gemini:latest` | Code Assist API (`cloudcode-pa.googleapis.com`) | `GEMINI_REFRESH_TOKEN`, `GEMINI_OAUTH_CLIENT_ID`, `GEMINI_OAUTH_CLIENT_SECRET` |
| codex | `intercom-agent-codex:latest` | `codex exec` CLI | `CODEX_OAUTH_ACCESS_TOKEN`, `CODEX_OAUTH_REFRESH_TOKEN`, `CODEX_OAUTH_ID_TOKEN`, `CODEX_OAUTH_ACCOUNT_ID` |

### Container Protocol

All containers speak the same stdin/stdout protocol:

**Input** — JSON on stdin: `{ "prompt", "sessionId", "groupFolder", "chatJid", "isMain", "model?", "secrets" }`

**Output** — JSON wrapped in sentinel markers on stdout:
```
---INTERCOM_OUTPUT_START---
{"status":"success","result":"response text","newSessionId":"...","event":{...}}
---INTERCOM_OUTPUT_END---
```

**Stream events**: `event` field carries `tool_start` (toolName, toolInput) and `text_delta` (text) for real-time streaming to Telegram via `StreamAccumulator`.

**IPC** — filesystem-based follow-up messages:
- Inbound: `/workspace/ipc/input/{timestamp}.json`, close sentinel: `_close`
- Outbound: `/workspace/ipc/messages/`, `/workspace/ipc/tasks/`, `/workspace/ipc/queries/` + `responses/`

### Runtime-Specific Details

**Claude** (`container/agent-runner/`): Agent SDK, per-group `.claude/` dir, supports swarms, MCP tools, auto-memory.

**Gemini** (`container/gemini-runner/`): Code Assist API at `cloudcode-pa.googleapis.com/v1internal`, OAuth refresh via `google-auth-library`, model `gemini-3.1-pro`, sessions as `Content[]` JSON, thinking parts filtered.

**Codex** (`container/codex-runner/`): wraps `codex exec` CLI, model `gpt-5.3-codex`, auth via `~/.codex/auth.json`, system prompt as `AGENTS.md`, flags `--skip-git-repo-check --ephemeral --dangerously-bypass-approvals-and-sandbox`.

## IronClaw (Rust Daemon)

### Crate Structure

| Crate | Purpose |
|-------|---------|
| `intercomd` | Axum HTTP daemon — Telegram bridge, IPC, events, orchestrator, container runner |
| `intercom-core` | Shared types: config, IPC, container protocol, Postgres persistence, Demarch adapter |
| `intercom-compat` | Legacy SQLite inspection and SQLite-to-Postgres migration |

### Configuration (`config/intercom.toml`)

TOML-based config with env var overrides (`INTERCOMD_BIND`, `INTERCOM_POSTGRES_DSN`, `HOST_CALLBACK_URL`). Key sections:

- `[server]` — bind address (default `127.0.0.1:7340`), host callback URL (default `http://127.0.0.1:7341`)
- `[storage]` — Postgres DSN, legacy SQLite path, groups dir
- `[runtimes]` — runtime profiles (claude/gemini/codex) with provider, default model, required env vars
- `[orchestrator]` — `enabled` flag, max concurrent containers, poll interval, idle timeout
- `[scheduler]` — `enabled` flag, poll interval, IANA timezone for cron
- `[events]` — `enabled` flag, poll interval, notification JID for push notifications
- `[demarch]` — `enabled` flag, read/write allowlists for `ic`/`bd` CLI commands

### CLI Subcommands

```bash
intercomd serve --config config/intercom.toml     # Start HTTP service (default)
intercomd print-config --config config/intercom.toml  # Dump effective config as JSON
intercomd inspect-legacy --sqlite store/messages.db   # Inspect legacy SQLite state
intercomd migrate-legacy --sqlite store/messages.db   # Migrate SQLite → Postgres
intercomd verify-migration --sqlite store/messages.db # Compare counts for parity
```

### HTTP API

| Endpoint | Purpose |
|----------|---------|
| `GET /healthz` | Health check with uptime |
| `GET /readyz` | Readiness: runtime profiles, Postgres, Telegram, orchestrator status |
| `GET /v1/runtime/profiles` | List configured runtime profiles |
| `POST /v1/telegram/ingress` | Route inbound Telegram message (trigger check, group lookup) |
| `POST /v1/telegram/send` | Send message via Telegram Bot API (with chunking) |
| `POST /v1/telegram/edit` | Edit existing Telegram message |
| `POST /v1/commands` | Handle slash commands (/help, /status, /model, /reset) |
| `POST /v1/demarch/read` | Execute Demarch read operation (allowlisted `ic`/`bd` commands) |
| `POST /v1/demarch/write` | Execute Demarch write operation (main group only) |
| `POST /v1/db/*` | 24 Postgres persistence endpoints (chats, messages, tasks, sessions, groups) |

### Background Loops

When `serve` is running, these loops run concurrently (shutdown via `tokio::sync::watch`):

1. **IPC watcher** — polls `data/ipc/{group}/` for messages, tasks, queries. Delegates messages/tasks to Node via `HttpDelegate`, handles Demarch queries natively.
2. **Group registry sync** — periodically fetches registered groups from Node host callback.
3. **Event consumer** — polls `ic events tail --consumer=intercom`, sends push notifications for `gate.pending`, `run.completed`, `budget.exceeded`, `phase.changed`.
4. **Message loop** (orchestrator) — polls Postgres for pending messages, dispatches to group queue.
5. **Scheduler** (orchestrator) — polls for due tasks, spawns containers for scheduled prompts.

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
npm run rust:test                         # Run Rust tests (129 tests)
npm test                                  # Run vitest (Node tests)
npm run typecheck                         # TypeScript type checking
cd container && bash build.sh latest all  # Build all container images
cd container && bash build.sh latest gemini  # Build single runtime
```

**Always restart services after building.** Compiled JS in `dist/` and the Rust binary are only loaded at process startup.

### Hot Reload

Runner source is bind-mounted from host into containers and recompiled on startup. Edit `container/*/src/*.ts` or `container/shared/*.ts` — changes take effect on next container spawn without rebuilding Docker images.

### Container Rebuild Rule

Rebuild container images after modifying runner source that changes dependencies or structure. Runner code changes (logic only) are picked up via hot reload.

```bash
cd container && bash build.sh latest <runtime>  # claude, gemini, codex, or all
```

### Rust-to-Node Wiring

Rust orchestration is always active (`orchestrator.enabled=true` in `intercom.toml`). Node routes Telegram ingress/egress through intercomd unconditionally with automatic fallback if unavailable. The host callback server starts on `HOST_CALLBACK_PORT` (default 7341).

## File Reference

### Host (`src/`)

| File | Purpose |
|------|---------|
| `index.ts` | Orchestrator: message loop, state management, agent dispatch |
| `config.ts` | Runtime selection, trigger pattern, paths, engine toggle |
| `types.ts` | RegisteredGroup, Channel, NewMessage, ScheduledTask interfaces |
| `container-runner.ts` | Container spawning, volume mounts, output streaming |
| `container-runtime.ts` | Docker/Podman detection, orphan cleanup |
| `mount-security.ts` | Allowlist-based mount validation |
| `group-queue.ts` | Per-group message queue with global concurrency limit |
| `channels/telegram.ts` | Telegram Bot API via Grammy |
| `channels/whatsapp.ts` | WhatsApp Web via Baileys |
| `host-callback.ts` | HTTP callback server for intercomd delegation |
| `intercomd-client.ts` | Client for intercomd bridge endpoints |
| `query-handlers.ts` | Demarch CLI query handlers (`ic`/`bd` via execFileSync) |
| `stream-accumulator.ts` | Real-time Telegram message editing with tool call streaming |
| `summarizer.ts` | Conversation summary caching (GPT-5.3 Codex) |
| `ipc.ts` | IPC watcher: messages, tasks, group registration |
| `router.ts` | Message formatting, channel selection, outbound routing |
| `task-scheduler.ts` | Cron/interval/once task scheduling and execution |
| `db.ts` | SQLite: messages, groups, sessions, state, tasks |

### Rust (`rust/`)

| File | Purpose |
|------|---------|
| `intercomd/src/main.rs` | Axum server, CLI, route wiring, shutdown coordination |
| `intercomd/src/telegram.rs` | Telegram bridge (ingress routing, send with chunking, edit) |
| `intercomd/src/ipc.rs` | IPC watcher, IpcDelegate trait, HttpDelegate, group registry |
| `intercomd/src/events.rs` | Kernel event consumer (gate, run, budget, phase notifications) |
| `intercomd/src/commands.rs` | Slash commands (/help, /status, /model, /reset) with model catalog |
| `intercomd/src/db.rs` | Postgres route handlers (24 endpoints) |
| `intercomd/src/queue.rs` | Group queue with concurrency limiting |
| `intercomd/src/message_loop.rs` | Message poll loop (orchestrator) |
| `intercomd/src/process_group.rs` | Container dispatch per group |
| `intercomd/src/scheduler.rs` | Task scheduler loop |
| `intercomd/src/scheduler_wiring.rs` | Scheduler callback wiring |
| `intercomd/src/container/runner.rs` | Async container spawning with OUTPUT marker streaming |
| `intercomd/src/container/mounts.rs` | Volume mount builder |
| `intercomd/src/container/secrets.rs` | Secret injection into containers |
| `intercomd/src/container/security.rs` | Mount allowlist validation |
| `intercom-core/src/config.rs` | TOML config with env overrides |
| `intercom-core/src/persistence.rs` | Postgres persistence (tokio-postgres) |
| `intercom-core/src/demarch.rs` | Demarch kernel adapter (ic/bd CLI execution) |
| `intercom-core/src/ipc.rs` | IPC types (IpcMessage, IpcTask, IpcQuery) |
| `intercom-core/src/container.rs` | Container protocol types and helpers |
| `intercom-compat/src/lib.rs` | SQLite inspection, migration, parity verification |

### Container (`container/`)

| File | Purpose |
|------|---------|
| `Dockerfile` / `Dockerfile.gemini` / `Dockerfile.codex` | Runtime images |
| `build.sh` | Multi-runtime build script |
| `agent-runner/src/index.ts` | Claude agent loop (Agent SDK) |
| `gemini-runner/src/index.ts` | Gemini agent loop (Code Assist API) |
| `codex-runner/src/index.ts` | Codex agent loop (codex exec CLI) |
| `shared/protocol.ts` | ContainerInput/Output types, OUTPUT markers |
| `shared/executor.ts` | Tool execution: shell, file, grep, glob |
| `shared/ipc-tools.ts` | IPC tools: send_message, schedule_task, register_group |
| `shared/ipc-input.ts` | IPC polling: drainIpcInput(), waitForIpcMessage() |
| `shared/session-base.ts` | Conversation archival (markdown transcripts) |
| `shared/system-prompt.ts` | System prompt builder |

## Security Model

- Agents run in Docker containers with filesystem isolation
- Each group gets its own IPC namespace (no cross-group message injection)
- Secrets passed via stdin, never written to mounted volumes
- Shell commands have secrets stripped from environment
- Additional mounts validated against external allowlist (`~/.config/intercom/mount-allowlist.json`)
- Non-main groups can be forced read-only via allowlist
- Hard policy block: `/wm` paths rejected for additional mounts
- Demarch writes restricted to main group by default (`require_main_group_for_writes`)
- Query handlers use `execFileSync` (no shell) to prevent command injection from container-supplied params
- Demarch read/write commands validated against allowlists in `intercom.toml`

## Gotchas

- **Container rebuild**: `--no-cache` doesn't invalidate COPY steps. Prune buildkit to force clean rebuild.
- **Hot reload**: Source mounted read-only and recompiled inside the container. Edit host files, not container files.
- **Gemini OAuth scope**: The Gemini CLI token has `cloud-platform` scope, not `generative-language`. Must use Code Assist API.
- **Codex auth.json format**: Rust parser is strict — needs all four fields: `id_token`, `access_token`, `refresh_token`, `account_id`.
- **Gemini thinking tokens**: Thinking parts (`thought: true`) count against maxOutputTokens and must be filtered from output.
- **Service restart order**: intercomd must start before intercom (configured via systemd `Before=` directive).
- **Orchestrator flag**: `orchestrator.enabled` defaults to false. When enabled, requires Postgres connection — logs a warning and disables itself if Postgres is unavailable.
- **Build then restart**: Both `npm run build` and `cargo build` produce artifacts loaded only at process startup. Always restart the corresponding service after building.

<!-- BEGIN BEADS INTEGRATION -->
## Issue Tracking with bd (beads)

**IMPORTANT**: This project uses **bd (beads)** for ALL issue tracking. Do NOT use markdown TODOs, task lists, or other tracking methods.

### Why bd?

- Dependency-aware: Track blockers and relationships between issues
- Git-friendly: Auto-syncs to JSONL for version control
- Agent-optimized: JSON output, ready work detection, discovered-from links
- Prevents duplicate tracking systems and confusion

### Quick Start

**Check for ready work:**

```bash
bd ready --json
```

**Create new issues:**

```bash
bd create "Issue title" --description="Detailed context" -t bug|feature|task -p 0-4 --json
bd create "Issue title" --description="What this issue is about" -p 1 --deps discovered-from:bd-123 --json
```

**Claim and update:**

```bash
bd update bd-42 --status in_progress --json
bd update bd-42 --priority 1 --json
```

**Complete work:**

```bash
bd close bd-42 --reason "Completed" --json
```

### Issue Types

- `bug` - Something broken
- `feature` - New functionality
- `task` - Work item (tests, docs, refactoring)
- `epic` - Large feature with subtasks
- `chore` - Maintenance (dependencies, tooling)

### Priorities

- `0` - Critical (security, data loss, broken builds)
- `1` - High (major features, important bugs)
- `2` - Medium (default, nice-to-have)
- `3` - Low (polish, optimization)
- `4` - Backlog (future ideas)

### Workflow for AI Agents

1. **Check ready work**: `bd ready` shows unblocked issues
2. **Claim your task**: `bd update <id> --status in_progress`
3. **Work on it**: Implement, test, document
4. **Discover new work?** Create linked issue:
   - `bd create "Found bug" --description="Details about what was found" -p 1 --deps discovered-from:<parent-id>`
5. **Complete**: `bd close <id> --reason "Done"`

### Auto-Sync

bd automatically syncs with git:

- Exports to `.beads/issues.jsonl` after changes (5s debounce)
- Imports from JSONL when newer (e.g., after `git pull`)
- No manual export/import needed!

### Important Rules

- ✅ Use bd for ALL task tracking
- ✅ Always use `--json` flag for programmatic use
- ✅ Link discovered work with `discovered-from` dependencies
- ✅ Check `bd ready` before asking "what should I work on?"
- ❌ Do NOT create markdown TODO lists
- ❌ Do NOT use external issue trackers
- ❌ Do NOT duplicate tracking systems

For more details, see README.md and docs/QUICKSTART.md.

<!-- END BEADS INTEGRATION -->

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
