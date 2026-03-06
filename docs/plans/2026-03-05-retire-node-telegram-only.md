# Retire Node Host — Telegram-Only Mode

**Bead:** iv-bpc5r
**Goal:** Move Telegram update polling into intercomd (Rust), eliminating the Node host process entirely. Single-binary deployment.

## Current Flow (Node + Rust)

```
Telegram API → grammY (Node) → SQLite + Postgres outbox → Rust outbox drain → message_loop → container → TelegramBridge (Rust) → Telegram API
```

Node is a pass-through: it receives updates, writes to DB, and Rust does everything else.

## Target Flow (Rust only)

```
Telegram API → Rust update poller → Postgres directly → message_loop → container → TelegramBridge → Telegram API
```

## What Node Does Today (and Rust equivalents)

| Node responsibility | Rust status | Action needed |
|---|---|---|
| Telegram `getUpdates` polling (grammY) | Not implemented | **Build**: ~300 lines, HTTP long-poll + JSON parse |
| Parse text messages | Rust ingress routing exists | Wire directly instead of via HTTP |
| Parse photos/documents + download media | Not implemented | **Build**: download via `getFile` API + save to group media dir |
| Parse non-text (video, voice, sticker, etc) | Not implemented | **Build**: placeholder tags like `[Video]`, `[Voice message]` |
| Slash commands (`/help`, `/model`, etc) | `commands.rs` fully implemented | Wire to update poller |
| Callback queries (button presses) | `handle_callback` fully implemented | Wire to update poller |
| `setMyCommands` (autocomplete menu) | Not implemented | **Build**: one API call at startup |
| `@bot_mention` → trigger translation | Rust `trigger_matches` exists | Add mention-to-trigger normalization |
| SQLite message storage | Being phased out (iv-sjz6t) | Drop — Postgres is primary |
| SQLite chat metadata | Being phased out | Drop — or add to Postgres |
| Outbox writer (pg-writer.ts) | Rust writes directly to Postgres | Drop — no outbox needed when Rust owns ingress |
| Host callback server | Bridge glue for Rust→Node sends | **Delete** — Rust sends directly |
| IPC watcher (Node side, already disabled) | Rust is IPC authority | Drop |
| Group registration (folder creation) | Rust has Postgres groups | Move folder creation to Rust |
| `intercomd-client.ts` (Node→Rust HTTP) | Bridge glue | **Delete** |

## Implementation Plan

### Phase 1: Telegram Update Poller in Rust
- [x] Add `telegram_poller` module to intercomd
- [x] Implement `getUpdates` long-polling loop with offset tracking
- [x] Parse `Update` types: `message` (text, photo, document, sticker, etc), `callback_query`
- [x] Store offset in memory (reset on restart is fine — Telegram replays missed updates)
- [x] Wire to existing trigger matching for message acceptance/rejection
- [x] For accepted messages: write directly to Postgres `messages` table + enqueue in GroupQueue
- [x] For commands: call `commands::handle_command` + apply effects + send response via TelegramBridge
- [x] For callbacks: call `handle_callback` (already wired)
- [x] Download media files (photos, documents) to `groups/<folder>/media/`
- [x] Translate `@bot_username` mentions to trigger pattern
- [x] Call `setMyCommands` at startup
- [x] Configurable polling timeout (default: 30s)

### Phase 2: Remove Node Dependencies
- [x] Add `TelegramDelegate` IPC delegate — sends via TelegramBridge directly
- [x] Auto-detect Telegram-only mode: skip `sync_registry_loop` when no Node host
- [x] Events consumer uses shared delegate (TelegramDelegate in Telegram-only mode)
- [x] IPC watcher uses `TelegramDelegate` for message sends (not HTTP to Node)

### Phase 3: Clean Up Node
- [x] Stop `intercom.service` (systemd)
- [ ] Remove `intercom.service` unit file
- [x] Archive Node source (don't delete — WhatsApp might return)
- [x] Update CLAUDE.md, AGENTS.md
- [ ] Update container build scripts if they reference Node

### Phase 4: Simplify Config
- [x] Remove `server.host_callback_url` from intercom.toml (commented out in example)
- [x] Remove `TELEGRAM_ONLY` env var (it's always Telegram-only now)
- [x] Remove outbox mode (no longer needed — Rust writes directly)
- [x] Simplify deployment docs (AGENTS.md updated)

## Key Design Decisions

1. **Long-polling, not webhooks** — matches current grammY behavior, works behind NAT, no SSL cert needed
2. **Direct Postgres write** — no outbox indirection when Rust owns both ingress and processing
3. **Keep SQLite reads for routing** — `route_ingress` already reads SQLite for group lookup. Can switch to Postgres-only after iv-sjz6t
4. **Media download via reqwest** — `getFile` returns a URL, download with reqwest, save to disk. Simple.
5. **Archive Node, don't delete** — WhatsApp (Baileys) might come back

## Risks

- **Telegram API edge cases** — grammY handles rate limiting, retries, flood control. We need to handle these in Rust. Start simple, add retry logic if needed.
- **Media download reliability** — Telegram file URLs expire after ~1 hour. Download immediately on receipt.
- **Update offset persistence** — If intercomd crashes mid-batch, some updates may replay. Telegram deduplication by message_id handles this (Postgres has `ON CONFLICT` on message inserts).

## Estimated Effort

- Phase 1: ~500 lines of Rust (the core work)
- Phase 2: ~200 lines of changes (mostly deletions)
- Phase 3: Config/docs cleanup
- Phase 4: Simplification

Total: one focused session.
