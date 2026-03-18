---
artifact_type: prd
bead: Demarch-mvy
stage: design
reviewed: true
reviewers: [fd-architecture, fd-user-product, fd-correctness]
---

# PRD: Intercom Go Rewrite

## Problem

Intercom's Rust+TypeScript+Docker architecture is isolated from Demarch's Go ecosystem (Skaffen, Clavain, Intercore, masaq). Every cross-pillar integration requires subprocess bridges and IPC JSON serialization, adding latency and maintenance burden. Docker containers add ~3s cold start and operational complexity for what are now thin wrappers around CLI tools.

## Solution

Rewrite Intercom entirely in Go. Replace the Rust daemon with a Go binary that imports Skaffen's router package directly (via thin adapter). Replace Docker container runners with persistent CLI subprocesses (`claude -p`, `gemini`, `codex exec`). Expose custom tools via a Go MCP server (Unix domain socket transport) that all CLI runtimes connect to.

## Pre-requisites (resolve before implementation)

1. **Telegram library spike**: Evaluate `tucnak/telebot` vs `go-telegram/bot` for inline keyboard API, callback routing, and streaming message edits. Commit before F3.
2. **MCP transport spike**: Verify `claude -p --mcp-server` flag behavior with UDS transport. Run: Go parent serves MCP on `/tmp/intercom-mcp.sock`, `claude -p --mcp-server /tmp/intercom-mcp.sock "list tools"`. Must pass before F4 starts.
3. **Skaffen import spike**: Test Go workspace mode with `go.work` linking Intercom and Skaffen modules. Confirm `router` package imports cleanly without pulling full OODARC type graph.

## Features

### F1: Go project scaffold + config
**What:** Initialize Go module, TOML config parsing, CLI entrypoint, systemd service file.
**Acceptance criteria:**
- [ ] `go build` produces `intercomd` binary
- [ ] Reads `config/intercom.toml` with env var overrides
- [ ] `intercomd serve` starts HTTP server on configured bind address
- [ ] `intercomd version` prints version info
- [ ] systemd service file updated for Go binary path

### F2: Postgres layer
**What:** Database connection pool, schema creation, and all persistence operations using pgx.
**Acceptance criteria:**
- [ ] `pgxpool.Pool` with configurable max connections
- [ ] `ensure_schema()` auto-creates all tables matching existing Rust DDL character-for-character (verify column names: `trigger_pattern` not `trigger`, etc.)
- [ ] All timestamps serialized with `time.RFC3339Nano` (sub-second precision required for cursor ordering)
- [ ] LISTEN/NOTIFY on `intercom_outbox` channel with: (a) immediate drain on every reconnect, (b) 30-second fallback poll, (c) startup recovery of stale `processing` rows to `pending`, (d) periodic stale recovery every 5 minutes
- [ ] All 24 existing Postgres endpoints ported (CRUD for each table)
- [ ] Existing Postgres data readable by new Go daemon (no migration needed)
- [ ] On startup, load existing sessions from `sessions` table into memory before first poll cycle (P0 — prevents context loss on first message)
- [ ] Schema compatibility test: verify all column names, types, and JSON field names against Rust DDL

### F3: Telegram bot
**What:** Telegram Bot API integration — long-polling, slash commands, message delivery with chunking, inline keyboards, callback handling.
**Acceptance criteria:**
- [ ] `getUpdates` long-polling with `update_id + 1` offset deduplication
- [ ] Slash commands: /help, /model, /reset, /status, /ping, /chatid, /new
- [ ] `setMyCommands` list matches implemented command set
- [ ] Message delivery with Telegram's 4096-char limit (chunking)
- [ ] Inline keyboard support for callback buttons
- [ ] Media handling (photos, documents, voice, stickers, location, contact — with placeholder strings for non-text)
- [ ] Message editing for streaming updates
- [ ] `@bot_username` mention translation to trigger string
- [ ] `<internal>` block stripping before Telegram delivery
- [ ] Unregistered chat handling (silent ignore, matching Rust behavior)
- [ ] Invalid bot token detection at startup (log error, exit poller)

### F4: Go MCP server
**What:** MCP server exposing Intercom's custom tools to CLI runtimes. Single server, Unix domain socket transport.
**Acceptance criteria:**
- [ ] Implements MCP protocol via `github.com/modelcontextprotocol/go-sdk`
- [ ] Intercom-domain tools: `send_message`, `schedule_task`, `list_tasks`, `pause_task`, `resume_task`, `cancel_task`, `register_group`
- [ ] Demarch read tools: `run_status`, `sprint_phase`, `search_beads`, `next_work`, `run_events`
- [ ] Demarch write tools (gated per group via config, disabled by default): `create_issue`, `start_run`, `approve_gate`, `reject_gate`
- [ ] Unix domain socket transport at configurable path (e.g., `/tmp/intercom-mcp-<group>.sock`)
- [ ] Tools have access to daemon state (Postgres pool, group registry) via shared references
- [ ] Every tool handler wrapped in `recover()` — panics return MCP error, never propagate to subprocess
- [ ] ~~`restart_service`~~ removed (footgun — user can restart via systemd directly)

### F5: Subprocess manager
**What:** Manages persistent per-group CLI subprocesses with lifecycle control, streaming output, and idle timeout.
**Acceptance criteria:**
- [ ] Spawns `claude -p`, `gemini`, or `codex exec` per group based on configured runtime
- [ ] Persistent process — stays alive between messages within a group
- [ ] Connects to MCP server via `--mcp-server` flag with UDS path
- [ ] Streams subprocess stdout to daemon for Telegram delivery
- [ ] Graceful shutdown: stop accepting Telegram updates first, then allow in-flight subprocesses to finish (configurable timeout, default 30s), then SIGTERM remaining
- [ ] Health monitoring — restart crashed subprocesses with exponential backoff (5s, 10s, 20s, 40s, 80s, cap at 5 min)
- [ ] 30-second watchdog: if no output within 30s of message dispatch, fast-fail and report error to user via Telegram
- [ ] `drain_pending()` after every subprocess exit — re-enqueue messages that arrived during active processing
- [ ] Session size guard: auto-reset session when JSONL exceeds `session_max_bytes` config
- [ ] Idle timeout reaper: re-check `active_delivery` within same lock after reap decision (prevents race with concurrent message arrival)
- [ ] Delivery channel receiver has context-based timeout — unblocks when subprocess exits
- [ ] Per-group working directory isolation (`groups/<folder>/`) with path traversal validation (reject `..`, `/`, `\`)
- [ ] Environment variable management (API keys, OAuth tokens per runtime)
- [ ] User sees error message in Telegram when subprocess fails (not silent no-response)

### F6: Skaffen router integration
**What:** Import Skaffen's router package for budget-aware model selection. Thin adapter to avoid OODARC type bleed.
**Acceptance criteria:**
- [ ] Go workspace mode (`go.work`) linking Intercom and Skaffen modules
- [ ] Thin `intercom/routing` adapter that calls `router.SelectModel()` with fixed phase (`tool.PhaseAct`), wraps result
- [ ] Local `BudgetTracker` struct delegating to `router.BudgetState()`
- [ ] Token usage tracking per group/session
- [ ] Budget state exposed via /status command
- [ ] Evidence emission via direct `ic events record` subprocess call (not via Skaffen's `evidence` package — avoids OODARC type dependency)
- [ ] `NoOpRouter` fallback when Skaffen unavailable (hardcoded model from config)

### F7a: Message queue + outbox
**What:** Per-group message queue with sequential processing and reliable outbox delivery.
**Acceptance criteria:**
- [ ] Single `sync.Mutex` protecting group state map (not `sync.Map`, not per-group channels)
- [ ] Sequential per-group processing: at most one active subprocess per group at any instant
- [ ] Correctness test: 100 concurrent messages to same group, atomic counter never exceeds 1
- [ ] Cursor advance via per-key UPSERT in `router_state` (one row per group, not full-map JSON serialization)
- [ ] Cursor rollback on failure without output sent
- [ ] `recover_pending_messages()` at startup: re-enqueue groups with unprocessed messages between cursor positions
- [ ] Tasks drain before messages within a group (priority ordering)
- [ ] Outbox drain via LISTEN/NOTIFY (F2 handles reconnect/fallback)

### F7b: Scheduler
**What:** Cron/interval/one-shot task scheduling with run logging.
**Acceptance criteria:**
- [ ] Cron expression scheduling (`robfig/cron`)
- [ ] Interval and one-shot scheduling
- [ ] Task run logging to Postgres (`task_run_logs` table)
- [ ] Tasks enqueue into F7a's message queue (not processed independently)
- [ ] Retry on transient failure: exponential backoff (5s, 10s, 20s, 40s, 80s), max 5 retries
- [ ] User sees error in Telegram when max retries exceeded

### F0 (after F2+F3): End-to-end smoke stub
**What:** Minimal message processing path for early integration testing.
**Acceptance criteria:**
- [ ] Telegram message received → enqueued → dispatched to no-op subprocess → response delivered to Telegram
- [ ] Validates F2+F3 integration before F4/F5 build begins

## Non-goals

- **WhatsApp support** — archived Node host stays as-is.
- **Admin TUI** — masaq integration deferred. CLI/HTTP admin sufficient.
- **OODARC phases** — Intercom imports `router` only, not the full agent FSM.
- **Skaffen `evidence` or `session` packages** — too much OODARC type bleed. Use `ic` CLI for evidence, local session tracking.
- **MCP plugin system** — Skaffen's `internal/mcp/` plugin loader not needed.
- **Backward compatibility with Rust daemon** — no maintenance after Go daemon validated.
- **New commands** (`/register`, `/schedule`) — parity first, new features after validation.

## Dependencies

- Skaffen `router` package importable via Go workspace
- `claude`, `gemini`, `codex` CLIs installed on host
- Postgres 15+ (existing `intercom-postgres` Docker container or host install)
- Go 1.24+ (matches Skaffen's go.mod)

## Validation criterion

After the Go daemon is live for 48 hours: zero missed responses, `/status` shows correct session IDs matching pre-cutover Postgres state. Port the existing 161+ Rust tests as Go equivalents for regression baseline.

## Flux-drive review log

Reviewed 2026-03-18 by fd-architecture, fd-user-product, fd-correctness. 27 findings incorporated:
- F6 scoped down to `router` only (OODARC type bleed)
- MCP transport changed from stdio to UDS (unverified assumption)
- F7 split into F7a (queue) + F7b (scheduler)
- F5 hardened: watchdog, backoff, drain_pending, reaper race fix, graceful shutdown
- F2 hardened: RFC3339Nano timestamps, LISTEN/NOTIFY reconnect drain, session reload on startup
- F3 corrected: added missing commands, bot mention translation, internal block stripping
- F4 scoped: removed restart_service, gated Demarch write tools
- Added F0 smoke stub for early integration testing
- Added pre-requisite spikes for Telegram lib, MCP transport, Skaffen import path
