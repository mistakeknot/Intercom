# NanoClaw — Developer & Agent Guide

Multi-runtime personal AI assistant with container isolation and messaging integration.

## Architecture Overview

```
Telegram / WhatsApp
        │
        ▼
   Host Process (Node.js)
   ├── channels/telegram.ts   Grammy long-polling, JID format: tg:{id}
   ├── channels/whatsapp.ts   Baileys WebSocket, JID format: {number}@s.whatsapp.net
   ├── index.ts                Message loop, state management, agent dispatch
   ├── container-runner.ts     Spawns Docker containers per runtime
   ├── group-queue.ts          Per-group concurrency with global limit
   ├── ipc.ts                  Filesystem-based IPC watcher
   ├── task-scheduler.ts       Cron/interval/once task execution
   └── db.ts                   SQLite (messages, groups, sessions, state)
        │
        ▼
   Docker Container (one per active conversation)
   ├── Claude runtime     → nanoclaw-agent:latest         (Claude Agent SDK)
   ├── Gemini runtime     → nanoclaw-agent-gemini:latest   (Code Assist API)
   └── Codex runtime      → nanoclaw-agent-codex:latest    (codex exec CLI)
```

## Multi-Runtime System

### Runtime Selection

Default runtime is set via `NANOCLAW_RUNTIME` env var in `.env` (values: `claude`, `gemini`, `codex`).

Per-group override: set `runtime` field on `RegisteredGroup` via IPC `register_group` command.

Resolution order: `group.runtime || DEFAULT_RUNTIME`.

### Container Images

| Runtime | Image | Backend | Auth |
|---------|-------|---------|------|
| claude | `nanoclaw-agent:latest` | Claude Agent SDK | `CLAUDE_CODE_OAUTH_TOKEN` |
| gemini | `nanoclaw-agent-gemini:latest` | Code Assist API (`cloudcode-pa.googleapis.com`) | `GEMINI_REFRESH_TOKEN`, `GEMINI_OAUTH_CLIENT_ID`, `GEMINI_OAUTH_CLIENT_SECRET` |
| codex | `nanoclaw-agent-codex:latest` | `codex exec` CLI | `CODEX_OAUTH_ACCESS_TOKEN`, `CODEX_OAUTH_REFRESH_TOKEN`, `CODEX_OAUTH_ID_TOKEN`, `CODEX_OAUTH_ACCOUNT_ID` |

### Container Protocol (shared by all runtimes)

All containers speak the same protocol regardless of LLM backend:

**Input** — JSON on stdin:
```json
{
  "prompt": "user message",
  "sessionId": "gemini-1234567890-abc123",
  "groupFolder": "main",
  "chatJid": "tg:1108701034",
  "isMain": true,
  "secrets": { "GEMINI_REFRESH_TOKEN": "...", ... }
}
```

**Output** — JSON wrapped in sentinel markers on stdout:
```
---NANOCLAW_OUTPUT_START---
{"status":"success","result":"response text","newSessionId":"gemini-1234567890-abc123"}
---NANOCLAW_OUTPUT_END---
```

**IPC** — filesystem-based follow-up messages:
- New messages written to `/workspace/ipc/input/{timestamp}.json`
- Close sentinel: `/workspace/ipc/input/_close`
- Outbound messages: `/workspace/ipc/messages/`
- Task commands: `/workspace/ipc/tasks/`

### Shared Code (`container/shared/`)

| File | Purpose |
|------|---------|
| `protocol.ts` | ContainerInput/Output types, OUTPUT markers, readStdin(), writeOutput() |
| `executor.ts` | Tool execution: shell (2min timeout, secret sanitization), file read/write/edit, grep, glob |
| `ipc-tools.ts` | NanoClaw IPC: send_message, schedule_task, list_tasks, pause/resume/cancel_task, register_group |
| `ipc-input.ts` | IPC polling: drainIpcInput(), waitForIpcMessage(), shouldClose() |
| `session-base.ts` | Conversation archival (markdown transcripts) |
| `system-prompt.ts` | System prompt builder (loads CLAUDE.md files, tool instructions) |

### Runtime-Specific Details

**Gemini** (`container/gemini-runner/`):
- Uses Google Code Assist API at `cloudcode-pa.googleapis.com/v1internal`
- OAuth refresh via `google-auth-library` — tokens cached in memory
- Model: `gemini-3.1-pro-preview`
- Project discovery via `loadCodeAssist` endpoint
- Tool calls handled inline (function call → execute → function response loop)
- Sessions persisted as Gemini `Content[]` JSON in `/workspace/group/.sessions/`
- Thinking parts (`thought: true`) filtered from output

**Codex** (`container/codex-runner/`):
- Wraps `codex exec` CLI (no SDK dependency)
- Model: `gpt-5.3-codex`
- Auth via `~/.codex/auth.json` written from container secrets
- System prompt written as `AGENTS.md` in working directory (Codex convention)
- Each query is a fresh `codex exec` invocation (stateless per query, context via prompt)
- Flags: `--skip-git-repo-check --ephemeral --dangerously-bypass-approvals-and-sandbox`

**Claude** (`container/agent-runner/`):
- Uses Claude Agent SDK directly
- Per-group `.claude/` directory with settings.json, skills, and sessions
- Most feature-rich: supports agent swarms, MCP tools, auto-memory

## Channel System

Channels implement the `Channel` interface (`src/types.ts`):
```typescript
interface Channel {
  name: string;
  connect(): Promise<void>;
  sendMessage(jid: string, text: string): Promise<void>;
  isConnected(): boolean;
  ownsJid(jid: string): boolean;
  disconnect(): Promise<void>;
  setTyping?(jid: string, isTyping: boolean): Promise<void>;
}
```

**JID routing**: Each channel's `ownsJid()` determines message routing. Telegram uses `tg:` prefix, WhatsApp uses the standard `@s.whatsapp.net` suffix.

**Telegram** (`src/channels/telegram.ts`):
- Grammy library for Bot API long polling
- Bot commands: `/chatid` (get registration JID), `/ping` (health check)
- 4096 char message splitting
- @bot_username mention → TRIGGER_PATTERN translation
- When `INTERCOM_ENGINE=rust`, Telegram ingress/egress is proxied through `intercomd` (`/v1/telegram/*`) with automatic fallback to the Node channel path if the bridge is unavailable

**WhatsApp** (`src/channels/whatsapp.ts`):
- Baileys library for WhatsApp Web protocol
- QR code auth via `npm run auth`

## Container Volumes

| Mount | Path | Purpose |
|-------|------|---------|
| Group folder | `/workspace/group` | Group's isolated workspace (read-write) |
| Project root | `/workspace/project` | Full project (main group only, read-write) |
| Global memory | `/workspace/global` | Shared CLAUDE.md (non-main, read-only) |
| IPC namespace | `/workspace/ipc` | Per-group IPC (read-write) |
| Runner source | `/app/{runner}/src` | Hot-reload: host source mounted in (read-only) |
| Shared code | `/app/shared` | Non-Claude runtimes only (read-only) |
| Claude sessions | `/home/node/.claude` | Claude runtime only (read-write) |
| Extra mounts | `/workspace/extra/*` | Additional mounts from containerConfig |

## Agent Tools (Gemini & Codex runtimes)

| Tool | Description |
|------|-------------|
| `run_shell_command` | Execute shell commands (120s timeout, secrets stripped from env) |
| `read_file` | Read file with line numbers, offset/limit |
| `write_file` | Write/overwrite file, creates parent dirs |
| `edit_file` | Replace unique string in file |
| `grep_search` | Regex search in files |
| `glob_files` | Find files by glob pattern |
| `list_directory` | List directory contents |
| `send_message` | Send message to user/group via IPC |
| `schedule_task` | Schedule cron/interval/once task |
| `list_tasks` | List scheduled tasks |
| `pause_task` / `resume_task` / `cancel_task` | Task lifecycle |
| `register_group` | Register new messaging group (main only) |

## Configuration (`.env`)

```bash
# Runtime (claude, gemini, codex)
NANOCLAW_RUNTIME=gemini

# Telegram
TELEGRAM_BOT_TOKEN=...
TELEGRAM_ONLY=true    # Skip WhatsApp initialization

# Claude OAuth
CLAUDE_CODE_OAUTH_TOKEN=...

# Gemini OAuth (Code Assist API)
GEMINI_REFRESH_TOKEN=...
GEMINI_OAUTH_CLIENT_ID=...
GEMINI_OAUTH_CLIENT_SECRET=...

# Codex OAuth
CODEX_OAUTH_ACCESS_TOKEN=...
CODEX_OAUTH_REFRESH_TOKEN=...
CODEX_OAUTH_ID_TOKEN=...
CODEX_OAUTH_ACCOUNT_ID=...

# General
ASSISTANT_NAME=Amtiskaw
IDLE_TIMEOUT=1800000      # 30min container idle timeout
CONTAINER_TIMEOUT=1800000 # 30min hard timeout

# Rust bridge (optional)
INTERCOM_ENGINE=rust
INTERCOMD_URL=http://127.0.0.1:7340
```

## Development

### Build & Run

```bash
npm run dev                               # Host with hot reload
npm run build                             # Compile TypeScript
cd container && bash build.sh latest all  # Build all container images
cd container && bash build.sh latest gemini  # Build single runtime
npm run rust:check                        # Check Rust migration workspace
npm run rust:build                        # Build Rust workspace
npm run rust:test                         # Run Rust workspace tests
```

**Always restart the service after building:** `systemctl --user restart intercom`. The compiled JS in `dist/` is only loaded at process startup — a build without a restart means the running service still uses the old code.

The Node service remains default in this phase. To test Rust service wiring, set `INTERCOM_ENGINE=rust` and run setup after building `rust/intercomd`.

`intercomd` bridge endpoints currently available:
- `POST /v1/telegram/ingress`
- `POST /v1/telegram/send`
- `POST /v1/telegram/edit`
- `POST /v1/demarch/read`
- `POST /v1/demarch/write`

### Hot Reload

Runner source code is bind-mounted from host into the container and recompiled on startup. This means you can edit `container/gemini-runner/src/*.ts` or `container/shared/*.ts` and the changes take effect on the next container spawn without rebuilding Docker images.

### Testing

```bash
npm test           # Run vitest
npm run typecheck  # TypeScript type checking
```

### Container Smoke Test

```bash
# Test Gemini runtime directly
echo '{"prompt":"What is 2+2?","groupFolder":"test","chatJid":"test","isMain":false,"secrets":{"GEMINI_REFRESH_TOKEN":"...","GEMINI_OAUTH_CLIENT_ID":"...","GEMINI_OAUTH_CLIENT_SECRET":"..."}}' \
  | docker run -i nanoclaw-agent-gemini:latest
```

## File Reference

### Host (`src/`)

| File | Purpose |
|------|---------|
| `index.ts` | Orchestrator: message loop, state management, agent dispatch |
| `config.ts` | Runtime selection, trigger pattern, paths, intervals, channel config |
| `types.ts` | RegisteredGroup, Channel, NewMessage, ScheduledTask interfaces |
| `container-runner.ts` | Container spawning, volume mounts, output streaming |
| `container-runtime.ts` | Docker/Podman detection, orphan cleanup |
| `mount-security.ts` | Allowlist-based mount validation |
| `group-queue.ts` | Per-group message queue with global concurrency limit |
| `channels/telegram.ts` | Telegram Bot API via Grammy |
| `channels/whatsapp.ts` | WhatsApp Web via Baileys |
| `ipc.ts` | IPC watcher: messages, tasks, group registration |
| `router.ts` | Message formatting, channel selection, outbound routing |
| `task-scheduler.ts` | Cron/interval/once task scheduling and execution |
| `db.ts` | SQLite: messages, groups, sessions, state, tasks |
| `env.ts` | .env file reader |
| `logger.ts` | Pino logger configuration |

### Container (`container/`)

| File | Purpose |
|------|---------|
| `Dockerfile` | Claude runtime image |
| `Dockerfile.gemini` | Gemini runtime image |
| `Dockerfile.codex` | Codex runtime image |
| `build.sh` | Multi-runtime build script |
| `agent-runner/src/index.ts` | Claude agent loop (Agent SDK) |
| `gemini-runner/src/index.ts` | Gemini agent loop (Code Assist API) |
| `gemini-runner/src/auth.ts` | Gemini OAuth token refresh |
| `gemini-runner/src/tools.ts` | Gemini tool declarations |
| `codex-runner/src/index.ts` | Codex agent loop (codex exec CLI) |
| `shared/*.ts` | Protocol, executor, IPC, sessions, system prompt |

## Security Model

- Agents run in Docker containers with filesystem isolation
- Each group gets its own IPC namespace (no cross-group message injection)
- Secrets passed via stdin, never written to mounted volumes
- Shell commands have secrets stripped from environment
- Additional mounts validated against external allowlist (`~/.config/nanoclaw/mount-allowlist.json`)
- Non-main groups can be forced read-only via allowlist

## Gotchas

- **Gemini OAuth scope**: The Gemini CLI token has `cloud-platform` scope, not `generative-language`. Standard Gemini API rejects it. Must use Code Assist API.
- **Codex auth.json format**: Rust parser is strict — needs all four fields: `id_token`, `access_token`, `refresh_token`, `account_id`.
- **Container build cache**: `--no-cache` doesn't invalidate COPY steps. Prune buildkit to force clean rebuild.
- **Hot reload**: Source is mounted read-only and recompiled inside the container. Edit host files, not container files.
- **Gemini thinking tokens**: Thinking parts (`thought: true`) count against maxOutputTokens and must be filtered from output.
