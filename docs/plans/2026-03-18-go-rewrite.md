---
artifact_type: plan
bead: Demarch-mvy
stage: design
requirements:
  - F0: End-to-end smoke stub
  - F1: Go project scaffold + config
  - F2: Postgres layer
  - F3: Telegram bot
  - F4: Go MCP server
  - F5: Subprocess manager
  - F6: Skaffen router integration
  - F7a: Message queue + outbox
  - F7b: Scheduler
---
# Intercom Go Rewrite — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use clavain:executing-plans to implement this plan task-by-task.

**Bead:** Demarch-mvy
**Phase:** planned (as of 2026-03-18T02:52:29Z)
**Goal:** Replace Intercom's Rust+TypeScript+Docker stack with a single Go binary using CLI subprocesses and a Go MCP server.

**Architecture:** Go daemon (`intercomd`) with Axum-equivalent HTTP server (chi), Telegram poller, subprocess manager for CLI agents, Go MCP server (UDS transport), pgx Postgres pool, and Skaffen router adapter. Each registered group gets a persistent CLI subprocess (`claude -p`, `gemini`, `codex exec`) that connects to the MCP server for custom tools.

**Tech Stack:** Go 1.24, pgx/v5, BurntSushi/toml, modelcontextprotocol/go-sdk, Telegram bot lib (TBD by spike), Skaffen router (via go.work), cobra CLI.

---

## Must-Haves

**Truths** (observable behaviors):
- User sends Telegram message → receives AI response (same as Rust daemon)
- Slash commands (/help, /model, /reset, /status, /ping, /chatid) work identically
- Scheduled tasks fire on time and produce output in Telegram
- Session context persists across messages within a group
- Existing Postgres data (sessions, groups, messages) readable without migration

**Artifacts** (files with specific exports):
- [`go/cmd/intercomd/main.go`] — CLI entrypoint with `serve` and `version` subcommands
- [`go/internal/db/pool.go`] — `NewPool()`, `EnsureSchema()`, all CRUD operations
- [`go/internal/telegram/bot.go`] — `NewBot()`, `Run()`, command handlers
- [`go/internal/mcp/server.go`] — `NewServer()`, tool registrations
- [`go/internal/subprocess/manager.go`] — `NewManager()`, `Dispatch()`, lifecycle control
- [`go/internal/queue/queue.go`] — `NewQueue()`, `Enqueue()`, sequential drain
- [`go/internal/routing/adapter.go`] — `NewRouter()`, `SelectModel()`

**Key Links** (connections where breakage cascades):
- Telegram poller feeds into Queue, Queue dispatches to SubprocessManager
- SubprocessManager connects to MCP server for tool access
- MCP server reads/writes Postgres via shared pool
- Router adapter selects model before subprocess spawn

---

## Pre-requisite Spikes (before Task 1)

### Spike A: Telegram library evaluation

**Step 1:** Create test program comparing `tucnak/telebot` and `go-telegram/bot`
```bash
mkdir -p /tmp/telegram-spike && cd /tmp/telegram-spike
go mod init telegram-spike
go get gopkg.in/telebot.v4
go get github.com/go-telegram/bot
```

**Step 2:** Test inline keyboard + callback + message edit with each. Pick winner.

**Step 3:** Record decision in brainstorm doc.

### Spike B: MCP transport verification

**Step 1:** Write minimal Go MCP server on UDS:
```go
// /tmp/mcp-spike/main.go
srv := mcp.NewServer("intercom", "0.1.0")
srv.AddTool("ping", "test connectivity", func(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
    return &mcp.CallToolResult{Content: []mcp.Content{{Type: "text", Text: "pong"}}}, nil
})
listener, _ := net.Listen("unix", "/tmp/intercom-mcp-test.sock")
srv.ServeListener(listener)
```

**Step 2:** Test with `claude -p --mcp-config '{"mcpServers":{"intercom":{"url":"unix:///tmp/intercom-mcp-test.sock"}}}' "call the ping tool"`

**Step 3:** If `--mcp-config` doesn't support UDS, try writing a temporary `mcp.json` file and pointing `claude` at it. Document the working approach.

### Spike C: Skaffen import path

**Step 1:** Create `go.work` at monorepo root:
```
go 1.24
use ./os/Skaffen
use ./apps/Intercom/go
```

**Step 2:** Write test import:
```go
import "github.com/mistakeknot/Skaffen/internal/router"
```

**Step 3:** If `internal/` blocks import, test: (a) adding Intercom as a package inside Skaffen module, or (b) creating `pkg/router/` facade in Skaffen. Document the working approach.

<verify>
- run: `ls /tmp/telegram-spike/go.mod 2>/dev/null || echo "spike not run yet"`
  expect: contains "telegram-spike"
</verify>

---

## Task 1: Go project scaffold + config [F1]

**Files:**
- Create: `go/go.mod`
- Create: `go/cmd/intercomd/main.go`
- Create: `go/internal/config/config.go`
- Create: `go/internal/config/config_test.go`
- Modify: `config/intercomd.service`

**Step 1: Initialize Go module**
```bash
mkdir -p apps/Intercom/go/cmd/intercomd apps/Intercom/go/internal/config
cd apps/Intercom/go
go mod init github.com/mistakeknot/intercom
go get github.com/BurntSushi/toml
go get github.com/spf13/cobra
```

**Step 2: Write config test**
```go
// go/internal/config/config_test.go
package config

import (
    "os"
    "testing"
)

func TestLoadConfig(t *testing.T) {
    tomlContent := `
[server]
bind = "127.0.0.1:7340"
request_timeout_ms = 30000

[storage]
postgres_dsn = "postgres://localhost/intercom"
groups_dir = "groups"

[runtimes.claude]
provider = "claude"
model = "claude-sonnet-4-6"
`
    f, _ := os.CreateTemp("", "intercom-*.toml")
    f.WriteString(tomlContent)
    f.Close()
    defer os.Remove(f.Name())

    cfg, err := Load(f.Name())
    if err != nil {
        t.Fatalf("Load failed: %v", err)
    }
    if cfg.Server.Bind != "127.0.0.1:7340" {
        t.Errorf("expected bind 127.0.0.1:7340, got %s", cfg.Server.Bind)
    }
    if cfg.Runtimes["claude"].Model != "claude-sonnet-4-6" {
        t.Errorf("expected model claude-sonnet-4-6, got %s", cfg.Runtimes["claude"].Model)
    }
}

func TestLoadConfigEnvOverride(t *testing.T) {
    tomlContent := `
[server]
bind = "127.0.0.1:7340"
[storage]
postgres_dsn = "postgres://localhost/intercom"
`
    f, _ := os.CreateTemp("", "intercom-*.toml")
    f.WriteString(tomlContent)
    f.Close()
    defer os.Remove(f.Name())

    os.Setenv("INTERCOMD_BIND", "0.0.0.0:8080")
    defer os.Unsetenv("INTERCOMD_BIND")

    cfg, err := Load(f.Name())
    if err != nil {
        t.Fatalf("Load failed: %v", err)
    }
    if cfg.Server.Bind != "0.0.0.0:8080" {
        t.Errorf("expected env override 0.0.0.0:8080, got %s", cfg.Server.Bind)
    }
}
```

**Step 3: Run test to verify it fails**
```bash
cd apps/Intercom/go && go test ./internal/config/ -v
```
Expected: FAIL — `Load` not defined

**Step 4: Write config implementation**
```go
// go/internal/config/config.go
package config

import (
    "os"
    "github.com/BurntSushi/toml"
)

type Config struct {
    Server      ServerConfig              `toml:"server"`
    Storage     StorageConfig             `toml:"storage"`
    Runtimes    map[string]RuntimeConfig  `toml:"runtimes"`
    Orchestrator OrchestratorConfig       `toml:"orchestrator"`
    Pool        PoolConfig                `toml:"pool"`
    Scheduler   SchedulerConfig           `toml:"scheduler"`
    Events      EventsConfig              `toml:"events"`
    Demarch     DemarchConfig             `toml:"demarch"`
}

type ServerConfig struct {
    Bind              string `toml:"bind"`
    RequestTimeoutMs  int    `toml:"request_timeout_ms"`
    MaxBodySize       int    `toml:"max_body_size"`
}

type StorageConfig struct {
    PostgresDSN string `toml:"postgres_dsn"`
    GroupsDir   string `toml:"groups_dir"`
}

type RuntimeConfig struct {
    Provider string   `toml:"provider"`
    Model    string   `toml:"model"`
    EnvVars  []string `toml:"required_env"`
}

type OrchestratorConfig struct {
    Enabled          bool `toml:"enabled"`
    MaxConcurrent    int  `toml:"max_concurrent"`
    UseOutbox        bool `toml:"use_outbox"`
    IdleTimeoutSec   int  `toml:"idle_timeout_sec"`
    ResultTimeoutSec int  `toml:"result_timeout_sec"`
    SessionMaxBytes  int  `toml:"session_max_bytes"`
}

type PoolConfig struct {
    Enabled        bool `toml:"enabled"`
    IdleTimeoutSec int  `toml:"idle_timeout_sec"`
    MaxContainers  int  `toml:"max_containers"`
}

type SchedulerConfig struct {
    Enabled      bool   `toml:"enabled"`
    PollInterval int    `toml:"poll_interval_sec"`
    Timezone     string `toml:"timezone"`
}

type EventsConfig struct {
    Enabled       bool   `toml:"enabled"`
    PushNotifyJID string `toml:"push_notification_jid"`
}

type DemarchConfig struct {
    Enabled                   bool     `toml:"enabled"`
    RequireMainGroupForWrites bool     `toml:"require_main_group_for_writes"`
    ReadAllowlist             []string `toml:"read_allowlist"`
    WriteAllowlist            []string `toml:"write_allowlist"`
}

func Load(path string) (*Config, error) {
    var cfg Config
    if _, err := toml.DecodeFile(path, &cfg); err != nil {
        return nil, err
    }
    applyEnvOverrides(&cfg)
    applyDefaults(&cfg)
    return &cfg, nil
}

func applyEnvOverrides(cfg *Config) {
    if v := os.Getenv("INTERCOMD_BIND"); v != "" {
        cfg.Server.Bind = v
    }
    if v := os.Getenv("INTERCOM_POSTGRES_DSN"); v != "" {
        cfg.Storage.PostgresDSN = v
    }
}

func applyDefaults(cfg *Config) {
    if cfg.Server.Bind == "" {
        cfg.Server.Bind = "127.0.0.1:7340"
    }
    if cfg.Server.RequestTimeoutMs == 0 {
        cfg.Server.RequestTimeoutMs = 30000
    }
    if cfg.Storage.GroupsDir == "" {
        cfg.Storage.GroupsDir = "groups"
    }
    if cfg.Orchestrator.IdleTimeoutSec == 0 {
        cfg.Orchestrator.IdleTimeoutSec = 300
    }
    if cfg.Orchestrator.ResultTimeoutSec == 0 {
        cfg.Orchestrator.ResultTimeoutSec = 600
    }
    if cfg.Orchestrator.SessionMaxBytes == 0 {
        cfg.Orchestrator.SessionMaxBytes = 10 * 1024 * 1024
    }
}
```

**Step 5: Write CLI entrypoint**
```go
// go/cmd/intercomd/main.go
package main

import (
    "fmt"
    "net/http"
    "os"
    "github.com/spf13/cobra"
    "github.com/mistakeknot/intercom/internal/config"
)

var version = "dev"

func main() {
    var configPath string

    root := &cobra.Command{Use: "intercomd"}
    root.PersistentFlags().StringVar(&configPath, "config", "config/intercom.toml", "config file path")

    root.AddCommand(&cobra.Command{
        Use: "version", Short: "Print version",
        Run: func(cmd *cobra.Command, args []string) { fmt.Println(version) },
    })

    root.AddCommand(&cobra.Command{
        Use: "serve", Short: "Start the daemon",
        RunE: func(cmd *cobra.Command, args []string) error {
            cfg, err := config.Load(configPath)
            if err != nil {
                return fmt.Errorf("load config: %w", err)
            }
            mux := http.NewServeMux()
            mux.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
                w.Write([]byte("ok"))
            })
            fmt.Printf("intercomd %s listening on %s\n", version, cfg.Server.Bind)
            return http.ListenAndServe(cfg.Server.Bind, mux)
        },
    })

    if err := root.Execute(); err != nil {
        os.Exit(1)
    }
}
```

**Step 6: Run tests, verify pass**
```bash
cd apps/Intercom/go && go test ./internal/config/ -v
```
Expected: PASS

**Step 7: Build binary**
```bash
cd apps/Intercom/go && go build -o intercomd ./cmd/intercomd
./intercomd version
```
Expected: prints "dev"

**Step 8: Update systemd service**
Update `config/intercomd.service` ExecStart to point to Go binary.

**Step 9: Commit**
```bash
git add apps/Intercom/go/ && git commit -m "feat(intercom): F1 — Go project scaffold with config and CLI"
```

<verify>
- run: `cd apps/Intercom/go && go test ./internal/config/ -v`
  expect: exit 0
- run: `cd apps/Intercom/go && go build -o /tmp/intercomd-test ./cmd/intercomd && /tmp/intercomd-test version`
  expect: contains "dev"
</verify>

---

## Task 2: Postgres layer [F2]

**Files:**
- Create: `go/internal/db/pool.go`
- Create: `go/internal/db/schema.go`
- Create: `go/internal/db/messages.go`
- Create: `go/internal/db/groups.go`
- Create: `go/internal/db/sessions.go`
- Create: `go/internal/db/scheduler.go`
- Create: `go/internal/db/outbox.go`
- Create: `go/internal/db/pool_test.go`
- Create: `go/internal/db/schema_test.go`

**Step 1: Add pgx dependency**
```bash
cd apps/Intercom/go && go get github.com/jackc/pgx/v5
```

**Step 2: Write schema test**
Test that `EnsureSchema()` creates all tables and that column names match Rust DDL exactly (especially `trigger_pattern`, not `trigger`).

**Step 3: Implement schema**
Port the exact DDL from `persistence.rs` — all 8 tables, all indexes, the LISTEN/NOTIFY trigger on `message_outbox`. Use `CREATE TABLE IF NOT EXISTS`.

**Step 4: Implement CRUD operations**
Port all 24 Postgres endpoints from `persistence.rs` and `db.rs`. Use `time.RFC3339Nano` for all timestamp serialization. Key operations:
- `InsertMessage()`, `GetMessagesSince()`, `GetMessagesByChat()`
- `UpsertGroup()`, `GetGroup()`, `ListGroups()`, `DeleteGroup()`
- `GetSession()`, `SetSession()`, `DeleteSession()`
- `GetRouterState()`, `SetRouterState()` — per-key UPSERT (not full-map JSON)
- `InsertOutboxRow()`, `ClaimOutboxRow()`, `CompleteOutboxRow()`, `RecoverStaleOutbox()`
- `InsertTask()`, `GetDueTasks()`, `UpdateTaskNextRun()`, `InsertTaskRunLog()`

**Step 5: Implement LISTEN/NOTIFY listener**
- Reconnect drain: on every successful reconnect, send immediate drain signal
- Fallback poll: 30-second interval
- Startup recovery: mark stale `processing` rows as `pending`
- Periodic stale recovery: every 5 minutes

**Step 6: Implement session reload on startup**
Load all rows from `sessions` table into in-memory map before any other subsystem starts.

**Step 7: Schema compatibility test**
Write a test that connects to existing Intercom Postgres, reads each table's column names, and asserts they match the Go DDL.

**Step 8: Run tests, commit**
```bash
cd apps/Intercom/go && go test ./internal/db/ -v
git add apps/Intercom/go/internal/db/ && git commit -m "feat(intercom): F2 — Postgres layer with pgx, LISTEN/NOTIFY, session reload"
```

<verify>
- run: `cd apps/Intercom/go && go test ./internal/db/ -v -count=1`
  expect: exit 0
</verify>

---

## Task 3: Telegram bot [F3]

**Files:**
- Create: `go/internal/telegram/bot.go`
- Create: `go/internal/telegram/commands.go`
- Create: `go/internal/telegram/delivery.go`
- Create: `go/internal/telegram/bot_test.go`

**Step 1: Add Telegram dependency** (based on spike A result)

**Step 2: Implement bot core**
- `getUpdates` long-polling with `update_id + 1` deduplication
- `setMyCommands` at startup (matching implemented commands exactly)
- `@bot_username` mention translation to trigger string
- Unregistered chat silent ignore
- Invalid bot token detection (log error, exit poller)

**Step 3: Implement command handlers**
Port from `commands.rs`: /help, /model, /reset, /status, /ping, /chatid, /new. Each handler returns a `CommandEffect` (none, clear_session, etc.).

**Step 4: Implement message delivery**
- 4096-char chunking with markdown-aware split points
- `<internal>` block stripping before delivery
- Message editing for streaming updates
- Media placeholder strings (stickers → `[Sticker]`, voice → `[Voice message]`, etc.)
- Inline keyboard support for callback buttons

**Step 5: Tests and commit**
```bash
cd apps/Intercom/go && go test ./internal/telegram/ -v
git add apps/Intercom/go/internal/telegram/ && git commit -m "feat(intercom): F3 — Telegram bot with commands and delivery"
```

<verify>
- run: `cd apps/Intercom/go && go test ./internal/telegram/ -v`
  expect: exit 0
</verify>

---

## Task 4: End-to-end smoke stub [F0]

**Files:**
- Modify: `go/cmd/intercomd/main.go`
- Create: `go/internal/smoke/smoke_test.go`

**Step 1: Wire F2 + F3 together in `serve` command**
On startup: load config → open Postgres pool → ensure schema → load sessions → start Telegram poller. On message received: enqueue (for now, just log and echo back via Telegram).

**Step 2: Write integration test**
Start daemon, send test Telegram message (mock), verify response delivered.

**Step 3: Manual verification**
Start daemon with real bot token, send message from phone, confirm echo response.

**Step 4: Commit**
```bash
git add apps/Intercom/go/ && git commit -m "feat(intercom): F0 — end-to-end smoke stub wiring F2+F3"
```

<verify>
- run: `cd apps/Intercom/go && go build ./cmd/intercomd`
  expect: exit 0
</verify>

---

## Task 5: Go MCP server [F4]

**Files:**
- Create: `go/internal/mcp/server.go`
- Create: `go/internal/mcp/tools_intercom.go`
- Create: `go/internal/mcp/tools_demarch.go`
- Create: `go/internal/mcp/server_test.go`

**Step 1: Add MCP SDK dependency**
```bash
cd apps/Intercom/go && go get github.com/modelcontextprotocol/go-sdk
```

**Step 2: Implement MCP server with UDS transport**
Based on spike B result. Server runs as goroutine inside daemon. Creates UDS per group at configurable path.

**Step 3: Implement Intercom-domain tools**
Each handler wrapped in `recover()`. Tools: `send_message`, `schedule_task`, `list_tasks`, `pause_task`, `resume_task`, `cancel_task`, `register_group`. All access shared Postgres pool.

**Step 4: Implement Demarch tools**
Read tools: `run_status`, `sprint_phase`, `search_beads`, `next_work`, `run_events` — shell out to `bd`/`ic` CLI.
Write tools (gated per group): `create_issue`, `start_run`, `approve_gate`, `reject_gate`.

**Step 5: Tests and commit**
```bash
cd apps/Intercom/go && go test ./internal/mcp/ -v
git add apps/Intercom/go/internal/mcp/ && git commit -m "feat(intercom): F4 — Go MCP server with UDS transport and custom tools"
```

<verify>
- run: `cd apps/Intercom/go && go test ./internal/mcp/ -v`
  expect: exit 0
</verify>

---

## Task 6: Subprocess manager [F5]

**Files:**
- Create: `go/internal/subprocess/manager.go`
- Create: `go/internal/subprocess/process.go`
- Create: `go/internal/subprocess/manager_test.go`

**Step 1: Implement process lifecycle**
- `Spawn(group, runtime, mcpSocketPath)` → persistent `os/exec.Cmd`
- Stdout streaming via scanner goroutine → delivery channel
- Stdin writing for message dispatch
- SIGTERM on idle timeout, context cancel on daemon shutdown

**Step 2: Implement lifecycle controls**
- Exponential backoff restart (5s, 10s, 20s, 40s, 80s, cap 5min)
- 30-second watchdog (no output → fast-fail, report error to Telegram)
- `drain_pending()` after every process exit
- Session size guard (auto-reset when JSONL exceeds limit)
- Idle timeout reaper with re-check of `active_delivery` inside lock
- Delivery channel context timeout (unblock on process exit)
- Path traversal validation for group folders

**Step 3: Implement graceful shutdown**
On SIGTERM to daemon: stop Telegram poller → wait for in-flight subprocesses (30s configurable timeout) → SIGTERM remaining → cleanup UDS sockets.

**Step 4: Implement error reporting**
When subprocess crashes or watchdog fires, send error message to Telegram group.

**Step 5: Concurrency test**
Verify sequential-per-group invariant: spawn 100 concurrent dispatches to same group, assert atomic counter never exceeds 1.

**Step 6: Tests and commit**
```bash
cd apps/Intercom/go && go test ./internal/subprocess/ -v
git add apps/Intercom/go/internal/subprocess/ && git commit -m "feat(intercom): F5 — subprocess manager with lifecycle controls"
```

<verify>
- run: `cd apps/Intercom/go && go test ./internal/subprocess/ -v`
  expect: exit 0
</verify>

---

## Task 7: Skaffen router integration [F6]

**Files:**
- Create: `go/internal/routing/adapter.go`
- Create: `go/internal/routing/adapter_test.go`
- Create: `go/internal/routing/evidence.go`
- Modify: `go.work` (if needed)

**Step 1: Set up Go workspace** (based on spike C result)

**Step 2: Implement thin router adapter**
```go
// go/internal/routing/adapter.go
package routing

type Router interface {
    SelectModel(group string) (model string, reason string)
    RecordUsage(group string, inputTokens, outputTokens int)
    BudgetState(group string) (spent, max int, pct float64)
}

// SkaffentRouter wraps Skaffen's router.DefaultRouter with fixed phase
type SkaffenRouter struct { /* ... */ }

// NoOpRouter returns hardcoded model from config
type NoOpRouter struct { Model string }
```

**Step 3: Implement evidence emission**
Shell out to `ic events record --source=intercom` with relevant fields. Fire-and-forget (non-blocking).

**Step 4: Tests and commit**
```bash
cd apps/Intercom/go && go test ./internal/routing/ -v
git add apps/Intercom/go/internal/routing/ && git commit -m "feat(intercom): F6 — Skaffen router adapter with NoOp fallback"
```

<verify>
- run: `cd apps/Intercom/go && go test ./internal/routing/ -v`
  expect: exit 0
</verify>

---

## Task 8: Message queue + outbox [F7a]

**Files:**
- Create: `go/internal/queue/queue.go`
- Create: `go/internal/queue/queue_test.go`

**Step 1: Implement queue with single mutex**
```go
type Queue struct {
    mu     sync.Mutex
    groups map[string]*groupState
    dispatch func(group string, msg Message) error
}

type groupState struct {
    active   bool
    pending  []Message
}
```

**Step 2: Implement sequential drain**
At most one active goroutine per group. Tasks drain before messages (priority). Cursor advance via per-key UPSERT. Cursor rollback on failure without output.

**Step 3: Implement `recover_pending_messages()`**
On startup: scan groups, find messages between last_agent_timestamp and last_timestamp, re-enqueue.

**Step 4: Correctness test**
100 concurrent messages to same group, atomic counter assertion.

**Step 5: Tests and commit**
```bash
cd apps/Intercom/go && go test ./internal/queue/ -v -race
git add apps/Intercom/go/internal/queue/ && git commit -m "feat(intercom): F7a — message queue with sequential per-group processing"
```

<verify>
- run: `cd apps/Intercom/go && go test ./internal/queue/ -v -race`
  expect: exit 0
</verify>

---

## Task 9: Scheduler [F7b]

**Files:**
- Create: `go/internal/scheduler/scheduler.go`
- Create: `go/internal/scheduler/scheduler_test.go`

**Step 1: Add cron dependency**
```bash
cd apps/Intercom/go && go get github.com/robfig/cron/v3
```

**Step 2: Implement scheduler**
Poll `scheduled_tasks` for due tasks. Support cron, interval, one-shot. Enqueue into F7a's queue. Log runs to `task_run_logs`. Retry with exponential backoff (max 5). Report error to Telegram on max retries exceeded.

**Step 3: Tests and commit**
```bash
cd apps/Intercom/go && go test ./internal/scheduler/ -v
git add apps/Intercom/go/internal/scheduler/ && git commit -m "feat(intercom): F7b — scheduler with cron/interval/one-shot and retry"
```

<verify>
- run: `cd apps/Intercom/go && go test ./internal/scheduler/ -v`
  expect: exit 0
</verify>

---

## Task 10: Final integration + wiring

**Files:**
- Modify: `go/cmd/intercomd/main.go`

**Step 1: Wire all components in `serve` command**
Config → Postgres pool → Sessions → Router → Queue → Subprocess manager → MCP server → Telegram bot → Scheduler. Graceful shutdown in reverse order.

**Step 2: Full integration test**
Send message from Telegram → queue → subprocess (claude -p) → MCP tool call → response → Telegram delivery. Verify round-trip.

**Step 3: Port Rust test suite**
Migrate the 161+ Rust tests to Go equivalents as regression baseline.

**Step 4: Final commit**
```bash
git add apps/Intercom/go/ && git commit -m "feat(intercom): wire all components — Go rewrite complete"
```

<verify>
- run: `cd apps/Intercom/go && go test ./... -v -count=1`
  expect: exit 0
- run: `cd apps/Intercom/go && go build -o /tmp/intercomd-final ./cmd/intercomd && /tmp/intercomd-final version`
  expect: contains "dev"
</verify>
