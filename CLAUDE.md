# Intercom

Multi-runtime personal AI assistant. See [AGENTS.md](AGENTS.md) for full architecture and development guide.

## Quick Context

Single-binary Rust daemon (`intercomd`). Handles the full message lifecycle: Telegram update polling, message routing, Postgres persistence, container dispatch, scheduling, and Telegram delivery. Three runtime backends: **Claude** (Agent SDK), **Gemini** (Code Assist API), **Codex** (codex exec CLI). Each group has isolated filesystem and container sandbox.

Node host (`src/`) is archived — kept for potential WhatsApp (Baileys) revival but not running in production.

## Key Files

| File | Purpose |
|------|---------|
| `rust/intercomd/src/main.rs` | Axum server, CLI, route wiring, background loops |
| `rust/intercomd/src/telegram_poller.rs` | Telegram `getUpdates` polling, message/command/media handling |
| `rust/intercomd/src/telegram.rs` | Telegram Bridge: send, edit, buttons, callbacks |
| `rust/intercomd/src/process_group.rs` | Container orchestrator (message → container → response) |
| `rust/intercomd/src/commands.rs` | Slash commands: /help, /model, /reset, /status |
| `rust/intercomd/src/ipc.rs` | IPC watcher + TelegramDelegate |
| `rust/intercom-core/src/config.rs` | TOML config with env overrides |
| `container/shared/` | Protocol, executor, IPC tools shared by all runtimes |

## Development

```bash
npm run rust:build:release && systemctl --user restart intercomd  # Build + restart
npm run rust:test                        # Rust tests (161+ tests)
cd container && bash build.sh latest all # Build all container images (intercom-agent:*)
```

## Service Management

```bash
systemctl --user {start|stop|restart|status} intercomd  # Rust daemon (only service needed)
```

## Container Build Cache

`--no-cache` alone does NOT invalidate COPY steps. Prune buildkit to force clean rebuild.
