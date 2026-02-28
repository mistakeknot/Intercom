# Intercom

Multi-runtime personal AI assistant. See [AGENTS.md](AGENTS.md) for full architecture and development guide.

## Quick Context

Dual-process system: **Node host** handles messaging channels (WhatsApp, Telegram) and command routing, **IronClaw** (Rust daemon, `intercomd`) handles orchestration, container dispatch, scheduling, IPC polling, Telegram bridge, and Postgres persistence. Three runtime backends: **Claude** (Agent SDK), **Gemini** (Code Assist API), **Codex** (codex exec CLI). Each group has isolated filesystem and container sandbox.

## Key Files

| File | Purpose |
|------|---------|
| `src/index.ts` | Node host: channels, commands, host callback server |
| `src/host-callback.ts` | HTTP callback server for intercomd delegation |
| `src/intercomd-client.ts` | Client for intercomd bridge endpoints |
| `src/container-runner.ts` | Spawns containers, selects image by runtime |
| `src/config.ts` | Runtime selection, trigger pattern, constants |
| `rust/intercomd/src/main.rs` | Rust daemon: Axum server, CLI, routes |
| `rust/intercom-core/src/config.rs` | TOML config with env overrides |
| `container/shared/` | Protocol, executor, IPC tools shared by all runtimes |

## Development

```bash
npm run dev                              # Node host with hot reload
npm run build && systemctl --user restart intercom    # Build + restart Node
npm run rust:build:release && systemctl --user restart intercomd  # Build + restart Rust
cd container && bash build.sh latest all # Build all container images (intercom-agent:*)
npm test                                 # Node tests (vitest)
npm run rust:test                        # Rust tests (129+ tests)
```

## Service Management

```bash
systemctl --user {start|stop|restart|status} intercom   # Node host
systemctl --user {start|stop|restart|status} intercomd  # Rust daemon
```

## Container Build Cache

`--no-cache` alone does NOT invalidate COPY steps. Prune buildkit to force clean rebuild.
