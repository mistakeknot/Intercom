# Multi-Runtime System

## Runtime Selection

The estate default remains `claude`. A group can opt into another model with `/model`; selecting `gpt-6-astra` resolves the built-in `astra` profile onto the Codex container with `high` reasoning and the Standard service tier. Profile selection never changes the estate default.

## Container Images

| Runtime | Image | Backend | Auth |
|---------|-------|---------|------|
| claude | `intercom-agent:latest` | Claude Agent SDK | `CLAUDE_CODE_OAUTH_TOKEN` |
| gemini | `intercom-agent-gemini:latest` | Code Assist API (`cloudcode-pa.googleapis.com`) | `GEMINI_REFRESH_TOKEN`, `GEMINI_OAUTH_CLIENT_ID`, `GEMINI_OAUTH_CLIENT_SECRET` |
| codex | `intercom-agent-codex:latest` | `codex exec` 0.153.2 | `CODEX_OAUTH_ACCESS_TOKEN`, `CODEX_OAUTH_REFRESH_TOKEN`, `CODEX_OAUTH_ID_TOKEN`, `CODEX_OAUTH_ACCOUNT_ID` |

## Container Protocol

All containers speak the same stdin/stdout protocol:

**Input** — JSON on stdin: `{ "prompt", "sessionId", "groupFolder", "chatJid", "isMain", "model?", "reasoningEffort?", "serviceTier?", "secrets" }`

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

**Codex** (`container/codex-runner/`): wraps the pinned `codex exec` CLI, auth via `~/.codex/auth.json`, and writes the system prompt as `AGENTS.md`. The ordinary Codex profile remains `gpt-5.3-codex`; the opt-in Astra profile explicitly passes `gpt-6-astra`, `model_reasoning_effort=high`, and `service_tier=default` (Codex's spelling for Standard).
