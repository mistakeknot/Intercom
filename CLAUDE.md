# NanoClaw

Multi-runtime personal AI assistant. See [AGENTS.md](AGENTS.md) for full architecture and development guide.

## Quick Context

Single Node.js host process that connects to messaging channels (Telegram, WhatsApp), routes messages to LLM agents running in Docker containers. Three runtime backends: **Claude** (Agent SDK), **Gemini** (Code Assist API), **Codex** (codex exec CLI). Each group has isolated filesystem and container sandbox.

## Key Files

| File | Purpose |
|------|---------|
| `src/index.ts` | Orchestrator: state, message loop, agent invocation |
| `src/channels/telegram.ts` | Telegram bot via Grammy (long polling) |
| `src/channels/whatsapp.ts` | WhatsApp connection via Baileys |
| `src/config.ts` | Runtime selection, trigger pattern, paths, intervals |
| `src/container-runner.ts` | Spawns containers, selects image by runtime |
| `src/types.ts` | RegisteredGroup (with `runtime` field), Channel interface |
| `src/ipc.ts` | IPC watcher and task processing |
| `src/router.ts` | Message formatting and outbound routing |
| `src/db.ts` | SQLite operations |
| `container/shared/` | Protocol, executor, IPC tools shared by all runtimes |
| `container/agent-runner/` | Claude runtime (Agent SDK) |
| `container/gemini-runner/` | Gemini runtime (Code Assist API) |
| `container/codex-runner/` | Codex runtime (codex exec CLI) |

## Multi-Runtime

Set default runtime via `NANOCLAW_RUNTIME` env var (claude/gemini/codex). Override per-group via `runtime` field on `RegisteredGroup`.

Container images: `nanoclaw-agent:latest`, `nanoclaw-agent-gemini:latest`, `nanoclaw-agent-codex:latest`.

All runtimes share the same stdin/stdout IPC protocol (OUTPUT markers), tool set, and session management.

## Skills

| Skill | When to Use |
|-------|-------------|
| `/setup` | First-time installation, authentication, service configuration |
| `/customize` | Adding channels, integrations, changing behavior |
| `/debug` | Container issues, logs, troubleshooting |
| `/update` | Pull upstream NanoClaw changes, merge with customizations, run migrations |

## Development

```bash
npm run dev                              # Run with hot reload
npm run build                            # Compile TypeScript
cd container && bash build.sh latest all # Build all container images
cd container && bash build.sh latest gemini # Build single runtime
```

Runner source is mounted from host into container and recompiled on startup, so code changes take effect without rebuilding images.

Service management:
```bash
systemctl --user start intercom
systemctl --user stop intercom
systemctl --user restart intercom
```

## Container Build Cache

The container buildkit caches aggressively. `--no-cache` alone does NOT invalidate COPY steps. To force a truly clean rebuild, prune the builder then re-run `build.sh`.
