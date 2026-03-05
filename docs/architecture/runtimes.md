# Multi-Runtime System

## Runtime Selection

Default runtime set via `INTERCOM_RUNTIME` env var (values: `claude`, `gemini`, `codex`). Per-group override via `runtime` field on `RegisteredGroup`. Resolution: `group.runtime || DEFAULT_RUNTIME`.

## Container Images

| Runtime | Image | Backend | Auth |
|---------|-------|---------|------|
| claude | `intercom-agent:latest` | Claude Agent SDK | `CLAUDE_CODE_OAUTH_TOKEN` |
| gemini | `intercom-agent-gemini:latest` | Code Assist API (`cloudcode-pa.googleapis.com`) | `GEMINI_REFRESH_TOKEN`, `GEMINI_OAUTH_CLIENT_ID`, `GEMINI_OAUTH_CLIENT_SECRET` |
| codex | `intercom-agent-codex:latest` | `codex exec` CLI | `CODEX_OAUTH_ACCESS_TOKEN`, `CODEX_OAUTH_REFRESH_TOKEN`, `CODEX_OAUTH_ID_TOKEN`, `CODEX_OAUTH_ACCOUNT_ID` |

## Container Protocol

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

## Runtime-Specific Details

**Claude** (`container/agent-runner/`): Agent SDK, per-group `.claude/` dir, supports swarms, MCP tools, auto-memory.

**Gemini** (`container/gemini-runner/`): Code Assist API at `cloudcode-pa.googleapis.com/v1internal`, OAuth refresh via `google-auth-library`, model `gemini-3.1-pro`, sessions as `Content[]` JSON, thinking parts filtered.

**Codex** (`container/codex-runner/`): wraps `codex exec` CLI, model `gpt-5.3-codex`, auth via `~/.codex/auth.json`, system prompt as `AGENTS.md`, flags `--skip-git-repo-check --ephemeral --dangerously-bypass-approvals-and-sandbox`.
