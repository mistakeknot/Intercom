# Intercom — Vision Document

**Version:** 0.1
**Date:** 2026-02-22
**Status:** Draft (brainstorm-grade)
**See also:** [Demarch vision](../../../docs/demarch-vision.md), [Autarch vision](../../autarch/docs/autarch-vision.md), [Intercore roadmap](../../../core/intercore/docs/intercore-roadmap.md)

---

## The Core Idea

Intercom is the human gateway into the Demarch agency. Where Clavain provides the developer experience via CLI, and Autarch provides it via TUI, Intercom provides it via **messaging channels** — Telegram, WhatsApp, and whatever comes next.

But "messaging frontend" undersells what Intercom actually is. Intercom is the only Demarch module that simultaneously:

1. **Faces external humans** (not agents, not developers-at-a-terminal)
2. **Runs its own agent execution environment** (container-isolated, multi-runtime)
3. **Manages its own dispatch lifecycle** (group queues, concurrency, IPC)
4. **Supports multiple LLM backends** (Claude, Gemini, Codex — unified protocol)

This makes it architecturally different from the other Autarch apps. Bigend renders kernel state. Gurgeh drives PRD generation. Coldwine orchestrates tasks. Pollard finds research. All of them are *surfaces for the agency*. Intercom is a **second execution plane** — one that extends the agency's reach beyond the terminal and into the pockets and group chats of people who will never run `ic` or `/clavain:sprint`.

The vision: Intercom evolves from a standalone messaging assistant into the **agency's external interface** — the layer through which Demarch interacts with users, teams, and the outside world. Not by absorbing other modules, but by becoming a **consumer, translator, and distributor** of what the rest of the platform produces.

## Why This Matters

Demarch's current interaction model is agent-facing. Every module assumes the user is either:
- A developer running Claude Code (Clavain's world)
- An agent dispatched by the kernel (Intercore's world)
- A power user reading a TUI (Autarch's world)

None of these cover the most natural interaction mode for most people: **sending a message**. A PM who wants to check sprint status. A founder who wants to ask "what should we build next?" A team member who wants to review findings from the latest flux-drive run. Today, they'd need to SSH in and run CLI commands. With an evolved Intercom, they send a Telegram message.

This isn't about making Demarch "accessible to non-technical users." It's about making the agency's output *addressable from anywhere*. The research Pollard produces, the specs Gurgeh generates, the sprint status Bigend monitors, the reviews Interflux runs — all of it becomes queryable through a conversation.

## Current State

Intercom today (codenamed NanoClaw) is a capable but self-contained system:

```
Telegram / WhatsApp
        │
        ▼
   Host Process (Node.js)
   ├── channels/          Telegram (Grammy), WhatsApp (Baileys)
   ├── container-runner    Spawn containers per runtime
   ├── group-queue         Per-group serialization, global concurrency
   ├── ipc                 Filesystem-based agent↔host communication
   ├── task-scheduler      Cron/interval/once execution
   └── db                  SQLite (messages, groups, sessions, state)
        │
        ▼
   Docker Containers (one per active conversation)
   ├── Claude runtime      Agent SDK + MCP
   ├── Gemini runtime      Code Assist API
   └── Codex runtime       codex exec CLI
```

**What it does well:**
- Multi-runtime agent execution with a unified protocol
- Container isolation with mount security
- Per-group workspace separation
- Scheduled tasks with cron expressions
- Agent Swarms (Teams) with per-agent bot identities
- Hot-reload development via bind-mounted source

**What it doesn't do:**
- Know about the rest of Demarch (no kernel integration, no Clavain awareness)
- Share its conversation history with other modules
- Consume outputs from Interflux, Gurgeh, Pollard, or Interpath
- Participate in Intercore runs, phases, or gates
- Route messages based on the agency's current state

## The Evolution

Intercom's evolution has three horizons. Each builds on the last, and each produces immediate value without requiring the next.

### Horizon 1: Agency-Aware Assistant

**Goal:** Intercom can see what the agency is doing and answer questions about it.

Today, an Intercom agent can only access what's in its container's mounted filesystem. It knows nothing about the kernel's runs, Clavain's sprints, Pollard's research, or Gurgeh's specs. The first horizon gives Intercom's agents **read access to agency state**.

**Concrete capabilities:**

- **Sprint status via message.** "What's the current sprint?" → Intercom agent queries `ic run current --json`, formats the response conversationally. No TUI required.
- **Research on demand.** "What do we know about WebSocket performance?" → Agent calls Pollard's API (or future Interbus `discovery.query` intent) and returns findings with attribution.
- **Spec lookup.** "What are the requirements for the auth feature?" → Agent queries Gurgeh's spec artifacts via kernel state.
- **Review summary.** "How did the last code review go?" → Agent reads Interflux verdict files, synthesizes via Intersynth patterns.
- **Work prioritization.** "What should I work on next?" → Agent queries beads state, applies Internext's tradeoff analysis, returns ranked recommendations.

**Implementation approach:**

The container agents already have tool execution (shell commands, file I/O). Horizon 1 adds a **Demarch toolkit** — a set of agent tools that bridge to kernel and ecosystem state:

```
Container Agent
├── Existing tools (shell, file I/O, IPC)
└── New: Demarch tools
    ├── demarch_run_status     → ic run current/status --json
    ├── demarch_sprint_phase   → ic run phase --json
    ├── demarch_search_beads   → bd list/show
    ├── demarch_research       → Pollard query or ic discovery search
    ├── demarch_spec_lookup    → ic run artifact list + read
    ├── demarch_review_summary → read verdict files from last flux-drive
    └── demarch_next_work      → bd ready + tradeoff scoring
```

These tools are thin wrappers that call existing CLIs. They work in all three runtimes (Claude, Gemini, Codex) because they're implemented in the shared container code, not runtime-specific.

**What this changes:** Intercom goes from "isolated assistant" to "agency-aware assistant." Users can ask about the state of their projects through a message instead of a terminal.

### Horizon 2: Agency Participant

**Goal:** Intercom can trigger agency actions and receive agency events.

Horizon 1 is read-only. Horizon 2 makes Intercom a **write participant** in the agency's workflow.

**Concrete capabilities:**

- **Start sprints from chat.** "Let's refactor the auth module" → Intercom creates a bead, optionally starts an Intercore run, and reports the run ID back to the chat.
- **Advance phases on approval.** When a gate requires human approval, the agency sends a message to the user's chat. The user replies "approved" and the phase advances. No SSH required.
- **Route findings to chat.** When Interflux completes a review, Intercom receives the verdict summary and posts it to the relevant group. The user reads it in Telegram, not in a file.
- **Capture insights from conversation.** When a user says something insightful in chat, the agent can register it as a kernel discovery or append it to Interfluence's learnings corpus.
- **Budget alerts.** When a run approaches its token budget, Intercom notifies the user and offers to pause, extend, or cancel.

**Implementation approach:**

This horizon requires two new integration surfaces:

**a) Intent submission from containers.** New IPC tools that submit intents to the agency:

```
Container Agent
└── New: Agency action tools
    ├── demarch_create_issue    → bd create (via IPC → host → bd)
    ├── demarch_start_run       → ic run create (via IPC → host → ic)
    ├── demarch_approve_gate    → ic gate override (via IPC → host → ic)
    ├── demarch_register_finding → ic discovery submit
    └── demarch_advance_phase   → OS intent: advance-run
```

The container agent doesn't call `ic` directly (that would require kernel binary inside the container). Instead, it writes an IPC intent file, and the host process validates and executes it. This preserves the security boundary.

**b) Event subscription on the host.** The Intercom host process subscribes to kernel events and routes relevant ones to messaging channels:

```
Host Process
└── New: Event bridge
    ├── Poll ic events tail --consumer=intercom
    ├── Filter by event type + project
    ├── Route to appropriate chat group
    └── Format as conversational messages
```

The event bridge is a lightweight polling loop (like `ic events tail` with a consumer cursor) that translates kernel events into outbound messages. It respects the Autarch write-path contract: all mutations go through the OS (Clavain), not directly to the kernel.

**What this changes:** Intercom becomes a bidirectional bridge between the agency and its users. The agency can ask for human input via messaging. Humans can direct the agency via messaging. The terminal is no longer the only control surface.

### Horizon 3: Distributed Agency Surface

**Goal:** Intercom becomes the interface through which **teams** interact with the agency.

Horizons 1 and 2 serve a single user. Horizon 3 extends to **multiple users with different roles** interacting with the same agency through different channels and groups.

**Concrete capabilities:**

- **Role-based access.** Different groups get different views and permissions. The "Engineering" group can start runs and approve gates. The "Product" group can query specs and prioritize work. The "Stakeholders" group gets read-only status summaries.
- **Cross-channel continuity.** A conversation started in Telegram can be continued in WhatsApp or in the CLI. The kernel holds the shared state; Intercom provides the channel routing.
- **Multi-agent delegation.** A user message triggers multiple agents (via Agent Swarms) that coordinate through Intermute. The user sees a synthesized response, not the raw multi-agent chatter.
- **Scheduled reporting.** Weekly sprint summaries, daily standup digests, review completion notifications — all pushed to the right channel at the right time via Intercom's existing task scheduler.
- **Voice adaptation.** Interfluence learns each user's communication style from their message history. Responses adapt to be conversational (not CLI-formatted) and match the user's preferences.

**Implementation approach:**

Horizon 3 builds on the Interbus integration mesh (bead `iv-psf2`) and Intermute coordination:

```
                    ┌─────────────────────────┐
                    │     Intercom Host        │
                    │                          │
Telegram ──────────►│  Channel Router          │
WhatsApp ──────────►│  ├── Group permissions   │
(Future) ──────────►│  ├── Role mapping        │
                    │  └── Channel routing     │
                    │                          │
                    │  Event Bridge             │
                    │  ├── ic events consumer   │
                    │  ├── Interbus subscriber  │
                    │  └── Intermute consumer   │
                    │                          │
                    │  Container Orchestrator   │
                    │  ├── Multi-runtime agents │
                    │  ├── Swarm coordinator    │
                    │  └── Synthesis pipeline   │
                    └─────────────────────────┘
                              │
                    ┌─────────┴──────────┐
                    │   Demarch Platform  │
                    ├── Intercore (state) │
                    ├── Clavain (policy)  │
                    ├── Intermute (coord) │
                    ├── Interbus (events) │
                    └── Interverse (caps) │
```

**What this changes:** The agency is no longer a single-user CLI experience. It's a system that teams interact with through their existing communication tools.

## Integration Map

How Intercom connects to each Demarch module:

| Module | Direction | Integration Surface | Horizon |
|--------|-----------|-------------------|---------|
| **Intercore** | Read | `ic run`, `ic dispatch`, `ic events` for agent status queries | H1 |
| **Intercore** | Write | `ic run create`, `ic gate override` via IPC intents | H2 |
| **Intercore** | Subscribe | `ic events tail --consumer=intercom` for live updates | H2 |
| **Clavain** | Read | Sprint status, phase info via `ic` (same data, OS policy layer) | H1 |
| **Clavain** | Write | Intent submission: start sprint, advance phase | H2 |
| **Beads** | Read | `bd list`, `bd show`, `bd ready` for work items | H1 |
| **Beads** | Write | `bd create`, `bd close` from chat-initiated work | H2 |
| **Interflux** | Read | Verdict files from review runs | H1 |
| **Interflux** | Trigger | Dispatch flux-drive on code shared in chat | H2 |
| **Intersynth** | Read | Synthesized verdicts for multi-agent summaries | H1 |
| **Internext** | Read | Prioritized work recommendations | H1 |
| **Interpath** | Trigger | Generate roadmaps, changelogs from conversation | H2 |
| **Interfluence** | Read | Voice profiles for response adaptation | H3 |
| **Interfluence** | Write | Corpus updates from user message history | H3 |
| **Intermute** | Both | Agent messaging, thread coordination, session tracking | H3 |
| **Interbus** | Both | Intent publish/subscribe for cross-module events | H3 |
| **Interlock** | Read | File reservation status for conflict awareness | H2 |
| **Intermux** | Read | Agent health/activity for status queries | H1 |
| **Pollard** | Query | Research lookup via API or discovery pipeline | H1 |
| **Gurgeh** | Query | Spec artifact retrieval | H1 |

## Architectural Positioning

### Where Intercom Sits Today

```
Layer 3: Apps (Autarch)
├── Bigend     (monitoring surface)
├── Gurgeh     (PRD surface)
├── Coldwine   (orchestration surface)
├── Pollard    (research surface)
└── Intercom   (messaging surface + execution environment)   ← the odd one out
```

Intercom fits the Autarch pillar loosely. It's an app, it's Layer 3, it's swappable. But it breaks the Autarch contract in a fundamental way: **it's not a pure rendering surface**. It has its own execution environment (containers), its own dispatch model (group queues), its own persistence (SQLite with a different schema from the kernel), and its own agent lifecycle.

The other Autarch apps are getting *simpler* over time — extracting arbiter logic to the OS, becoming pure renderers of kernel state. Intercom is getting *more capable* — adding runtimes, channels, swarm coordination, scheduled execution.

### Should Intercom Become a Pillar?

**Not yet.** But it's worth tracking when the answer changes.

The threshold for pillar status is: **does this module represent a category of capability that other modules depend on, rather than a single application?**

Today, Intercom is an application. Nothing depends on it. You can remove it and the kernel, OS, drivers, and other apps are unaffected. That's the definition of an app.

The answer changes if Intercom evolves into a **communication substrate** — a layer that other modules route through to reach humans. If Clavain sends gate-approval requests through Intercom, if Interflux routes review summaries through Intercom, if Intercore sends budget alerts through Intercom... then Intercom is no longer an app. It's infrastructure. And infrastructure belongs in a different layer.

**Possible future positioning:**

```
Layer 3: Apps (Autarch)              ← TUI surfaces
├── Bigend, Gurgeh, Coldwine, Pollard

Layer 2.5: Gateway (Intercom)        ← external communication surface
├── Channel routing (Telegram, WhatsApp, ...)
├── Event bridge (kernel → messaging)
├── Intent translation (messaging → kernel)
└── Multi-runtime agent execution

Layer 2: OS (Clavain) + Drivers (Interverse)
Layer 1: Kernel (Intercore)
```

This "Layer 2.5" framing acknowledges that Intercom sits *between* the OS and the apps — it's not a pure consumer of kernel state (like Autarch), and it's not a policy engine (like Clavain). It's a **boundary layer** between the agency and the external world.

But this is a future consideration. The three horizons above don't require reclassification. Each horizon works with Intercom living under `apps/`.

## What Intercom Is Not

- **Not a replacement for the CLI.** Power users and agents will always use Clavain directly. Intercom serves people who don't live in a terminal.
- **Not an API gateway.** It doesn't expose REST endpoints for programmatic access. That's Intermute's role. Intercom is for conversational, human-to-agency interaction.
- **Not a notification service.** It's bidirectional. One-way alerting is a subset of what it does, not its purpose.
- **Not a new orchestrator.** It doesn't compete with Clavain's workflow or Coldwine's task coordination. It surfaces their outputs and translates user intent into their inputs.
- **Not required.** Like all Autarch apps, Intercom is optional. The agency runs fine without it. But with it, the agency becomes reachable from anywhere.

## Design Principles (Intercom-Specific)

### 1. Translate, don't duplicate

Intercom never reimplements agency logic. It translates between messaging and the existing agency interfaces. If Clavain changes how sprints work, Intercom's translation layer adapts — it doesn't maintain a parallel sprint model.

### 2. Containers are the security boundary

Every user message passes through a container sandbox. The host process handles channel I/O and event routing but never executes LLM-generated actions directly. This is Intercom's core safety invariant and must survive all evolution.

### 3. Degrade gracefully

Each horizon adds capability but none are prerequisites. Without kernel integration, Intercom is a standalone assistant. With H1, it can answer agency questions. With H2, it can act on the agency. With H3, it serves teams. Each level works independently.

### 4. Conversation-native, not CLI-over-chat

Intercom responses should be conversational, not formatted like terminal output. A sprint status update should read like a message from a colleague, not like piped JSON. Interfluence voice profiles (H3) accelerate this, but the principle applies from H1.

### 5. Channel-agnostic internally

The host process doesn't know or care whether a message came from Telegram or WhatsApp. The Channel interface abstracts this. New channels (Discord, Slack, email, SMS) plug in without touching the orchestration or agent layers.

## Open Questions

1. **Container access to kernel.** H1 tools need `ic` inside containers. Options: (a) mount the `ic` binary read-only, (b) IPC-bridge all queries through the host, (c) HTTP API on the host that containers can call. Each has different security/latency tradeoffs.

2. **Event delivery latency.** `ic events tail` is polling-based. For real-time messaging, should Intercom use Autarch's signal broker pattern, or is a simple poll loop sufficient?

3. **Multi-user identity.** H3 needs user identity (who sent this message). The kernel doesn't model individual users — it models runs and dispatches. Where does user identity live? Intercom's own DB? Intermute?

4. **Gate approval UX.** When a gate needs human approval, what does the message look like? Inline buttons (Telegram supports these)? A reply keyword? How does the system handle approval timeout?

5. **Interbus vs direct integration.** Should H2 wait for Interbus (`iv-psf2`), or proceed with direct `ic` calls and retrofit later? Direct integration ships faster but may need refactoring.

6. **Token attribution.** When an Intercom agent queries the kernel or runs tools, who "pays" for the tokens? The Intercom conversation? The kernel run? Both? This matters for budget enforcement.

---

*This is a brainstorm-grade vision document. It captures the direction, not the plan. Implementation details, sequencing, and commitment decisions happen in strategy and planning phases.*
