# Message Flow (Telegram-Only Mode)

In production, intercomd handles the full message lifecycle directly:

```
Telegram API (getUpdates long-polling)
        |
        v
  telegram_poller.rs: parse update, match trigger, download media
        |
        v
  Postgres: store_message() + store_chat_metadata()
        |
        v
  queue.rs: enqueue_message_check()
        |
        v
  process_group.rs: container dispatch (with typing indicator)
        |
        v
  TelegramBridge: send response chunks via Bot API
```

**Key properties:**
- **Direct write**: poller writes to Postgres immediately, no outbox indirection
- **At-least-once**: Telegram message_id used for deduplication (ON CONFLICT on inserts)
- **Media download**: photos/documents downloaded via `getFile` API before processing
- **Queue drain**: pending messages dispatched after every container completion via `drain_pending()`

## Legacy: Outbox Mode

When running with Node host (`use_outbox=true`), messages flowed through a durable outbox pipeline. This mode is disabled in Telegram-only mode. See git history for details.
