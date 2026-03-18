---
artifact_type: brainstorm
bead: Demarch-mvy
stage: discover
---

# Intercom Go Rewrite: Full Architecture Replacement

## What We're Building

Complete rewrite of Intercom from Rust+TypeScript+Docker to Go. The Go daemon replaces the Rust `intercomd` binary (~13K lines). The TypeScript container runners (~3.3K lines) are eliminated — Claude, Gemini, and Codex agents run as CLI subprocesses (`claude -p`, `gemini`, `codex exec`) managed directly by the Go daemon. Docker is removed entirely.

A new Go MCP server exposes Intercom's custom tools (messaging, scheduling, Demarch operations) to all three CLI runtimes via `--mcp-server`, replacing the per-container copies of these tools.

## Why This Approach

### The language boundary problem

Demarch's stack is overwhelmingly Go: Skaffen (220 Go files — provider, router, session, evidence, trust, agentloop, mcp), Clavain (clavain-cli), Intercore, masaq (Bubble Tea TUI), Autarch. Rust is the outlier — every Skaffen/Intercore integration requires subprocess bridges and IPC JSON serialization.

### What Go unlocks (direct imports, not subprocess bridges)

- **Skaffen router**: `router.SelectModel()` for budget-aware model routing with graceful degradation (Opus → Sonnet → Haiku)
- **Skaffen evidence**: `evidence.Emit()` for direct Intercore bridge (no `ic events record` subprocess)
- **Skaffen session**: `session.JSONLSession` for conversation persistence with compaction
- **masaq components**: Bubble Tea TUI for admin interface
- **MCP Go SDK**: `github.com/modelcontextprotocol/go-sdk` (already in Skaffen's go.mod)

### Why Docker is no longer needed

When the container runners were built, CLI-native agent modes didn't exist. Now all three providers have mature CLIs with full agent capability:

| Runtime | CLI | Tool execution | Session support |
|---------|-----|----------------|-----------------|
| Claude | `claude -p --output-format stream-json` | Full (Bash, Read, Write, Edit, web, subagents) | `--resume` flag |
| Gemini | `gemini` CLI | Full (equivalent agent mode) | Native sessions |
| Codex | `codex exec` | Full (already subprocess-based in current design) | `--ephemeral` or persistent |

The container runners are now just heavyweight wrappers around these CLIs, adding Docker overhead (~3s cold start) for isolation that the daemon can provide more cheaply via subprocess management.

## Key Decisions

### 1. Strategy A++ — full replacement, no Docker

Considered three strategies:
- **A (conservative)**: Rewrite daemon only, keep Docker containers → rejected: leaves the language boundary for agent execution
- **A+ (partial)**: Daemon + Claude via CLI, keep Gemini/Codex in Docker → rejected: incomplete
- **A++ (chosen)**: Daemon + all runtimes via CLI subprocess → eliminates Docker entirely, cleanest architecture

### 2. Go MCP server for custom tools

Currently each container runner has its own copy of IPC tools. In A++, a single Go MCP server exposes these once:
- `send_message` — async message delivery to any group
- `schedule_task` — cron/interval/once scheduling
- `list_tasks`, `pause_task`, `resume_task`, `cancel_task`
- `register_group`, `restart_service`
- Demarch queries: `run_status`, `sprint_phase`, `search_beads`, `next_work`, etc.
- Demarch writes: `create_issue`, `start_run`, `approve_gate`, etc.

All three CLIs connect via `--mcp-server` flag. This is a cleaner separation: daemon owns tools, CLIs own agent execution.

### 3. Persistent per-group subprocess lifecycle

Each registered group gets a long-lived CLI subprocess. Process stays alive between messages with configurable idle timeout. Session state preserved in-process. This subsumes the warm container pool work (iv-7iy1i).

### 4. No production cutover needed

The Rust daemon is dev-only — no production migration ceremony required. Build, test, deploy.

## Architecture Overview

```
                    Telegram Bot API
                         │
                    ┌─────┴─────┐
                    │  Go Daemon │
                    │ (intercomd)│
                    └─────┬─────┘
                          │
          ┌───────────────┼───────────────┐
          │               │               │
    ┌─────┴─────┐  ┌─────┴─────┐  ┌─────┴─────┐
    │ claude -p  │  │  gemini   │  │ codex exec │
    │ subprocess │  │ subprocess│  │ subprocess │
    └─────┬─────┘  └─────┬─────┘  └─────┬─────┘
          │               │               │
          └───────────────┼───────────────┘
                          │
                   ┌──────┴──────┐
                   │ Go MCP Srv  │
                   │ (tools)     │
                   └──────┬──────┘
                          │
              ┌───────────┼───────────┐
              │           │           │
         Postgres    Skaffen      Demarch
         (pgx)      (router,     (bd, ic
                    evidence,    direct Go
                    session)     imports)
```

## Module Mapping (Rust → Go)

| Rust module | Lines | Go replacement |
|-------------|-------|---------------|
| `main.rs` (HTTP server, CLI) | 1,009 | `net/http` or `chi` + `cobra` |
| `telegram_poller.rs` | 901 | `tucnak/telebot` or `go-telegram/bot` |
| `process_group.rs` (container orchestrator) | 883 | Go subprocess manager (os/exec) |
| `queue.rs` (per-group message queues) | 1,085 | Go channels + sync primitives |
| `ipc.rs` (filesystem IPC watcher) | 1,345 | `fsnotify` or polling |
| `container/runner.rs` | ~500 | Eliminated (CLI subprocesses) |
| `commands.rs` (slash commands) | 728 | Go handlers |
| `events.rs` (kernel consumer) | 658 | Direct Skaffen evidence import |
| `db.rs` (Postgres endpoints) | 574 | `pgx` |
| `persistence.rs` (schema + pool) | 1,629 | `pgx` + migration tool |
| `demarch.rs` (adapter) | 926 | Direct Go imports (Skaffen, beads) |
| `scheduler_wiring.rs` + `scheduler.rs` | 685 | `robfig/cron` or Go timer |
| `message_loop.rs` | 422 | Go goroutine |
| `outbox.rs` (LISTEN/NOTIFY) | 421 | `pgx` LISTEN/NOTIFY |
| `telegram.rs` (Bot API bridge) | 482 | Part of telebot lib |
| `config.rs` | 346 | `BurntSushi/toml` (already in Skaffen) |

## Go Dependencies (Expected)

Core (already in Skaffen ecosystem):
- `github.com/mistakeknot/Skaffen` — provider, router, evidence, session
- `github.com/mistakeknot/Masaq` — TUI components
- `github.com/BurntSushi/toml` — config
- `github.com/modelcontextprotocol/go-sdk` — MCP server for custom tools

New:
- `github.com/jackc/pgx/v5` — Postgres (async, LISTEN/NOTIFY, connection pool)
- `github.com/tucnak/telebot` or `github.com/go-telegram/bot` — Telegram Bot API
- `github.com/robfig/cron/v3` — cron expression scheduling
- `github.com/fsnotify/fsnotify` — filesystem watching (if needed for IPC compat)

## Open Questions

1. **Telegram library choice**: `tucnak/telebot` (~4.5K stars, framework-level DX, closest to grammY) vs `go-telegram/bot` (newer, Bot API v9.4, zero deps). Need to evaluate inline keyboard support, callback handling, and chunked message sending.

2. **MCP server transport**: stdio (simplest, one server per CLI subprocess) vs HTTP (single server, all subprocesses connect). stdio is simpler but means N server instances. HTTP means one server but requires port management.

3. **Session continuity across model switches** (existing bug iv-elbnh): The Go rewrite should fix this by design — the daemon owns session state, CLI subprocesses are execution engines. When user switches models, daemon spawns new CLI subprocess but provides the same conversation context.

4. **Skaffen import path**: `internal/` packages can't be imported by external modules. Either restructure Skaffen to expose `pkg/` interfaces, or make Intercom a package within the Skaffen module, or use Go workspace mode.

5. **Beads for subtasks**: This epic needs children for each major module (Telegram, Postgres, MCP server, subprocess manager, etc.). Create during planning phase.
