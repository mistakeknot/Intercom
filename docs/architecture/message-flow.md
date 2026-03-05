# Message Flow (Outbox Mode)

When `orchestrator.use_outbox=true` (production default), messages flow through a durable outbox pipeline:

```
WhatsApp/Telegram message arrives
        |
        v
  Node Host (channel layer)
        |
        v
  pg-writer.ts: INSERT INTO message_outbox
        |
        v
  Postgres trigger: NOTIFY intercom_outbox
        |
        v
  outbox.rs LISTEN loop: receives notification
        |
        v
  outbox.rs drain loop: claim rows → store_message() → queue.enqueue_message_check()
        |
        v
  process_group.rs: container dispatch (with typing indicator)
```

**Key properties:**
- **Durable**: messages survive Node/Rust restarts (persisted in Postgres before processing)
- **At-least-once**: stale `processing` rows recovered at startup and every 5 minutes
- **Low latency**: LISTEN/NOTIFY delivers within ~50ms; 30s fallback poll as safety net
- **Cleanup**: hourly loop deletes delivered rows older than 7 days

**Outbox row lifecycle:** `pending` → `processing` (claimed) → `delivered` | `failed` | back to `pending` (retry)

**Payload types:** `message` (chat messages) and `chat_metadata` (chat name/channel updates)

**Legacy mode** (`use_outbox=false`): Rust polls the `messages` table directly via `message_loop.rs`. The two modes are mutually exclusive.
