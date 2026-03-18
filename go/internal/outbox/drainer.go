// Package outbox implements the outbox drain loop.
//
// The outbox is a durable write path: external writers insert into message_outbox,
// Postgres fires NOTIFY, the drainer claims rows and dispatches by payload_type.
package outbox

import (
	"context"
	"encoding/json"
	"log/slog"
	"time"

	"github.com/mistakeknot/intercom/internal/db"
	"github.com/mistakeknot/intercom/internal/queue"
)

const (
	drainClaimLimit       = 10
	drainFallbackInterval = 30 * time.Second
	staleRecoveryInterval = 5 * time.Minute
	cleanupInterval       = 1 * time.Hour
	cleanupRetentionDays  = 7
)

// Drainer processes outbox rows, dispatching by payload_type.
type Drainer struct {
	pool  *db.Pool
	queue *queue.Queue
}

func NewDrainer(pool *db.Pool, queue *queue.Queue) *Drainer {
	return &Drainer{pool: pool, queue: queue}
}

// Run waits for drain signals (from LISTEN/NOTIFY) or a fallback poll timer,
// then claims and processes outbox rows. Blocks until ctx is cancelled.
func (d *Drainer) Run(ctx context.Context, drainSignal <-chan struct{}) error {
	// Recover stale rows from prior crash
	if count, err := d.pool.RecoverStaleOutboxRows(ctx); err != nil {
		slog.Warn("failed to recover stale outbox rows", "err", err)
	} else if count > 0 {
		slog.Info("recovered stale outbox rows at startup", "count", count)
	}

	fallback := time.NewTicker(drainFallbackInterval)
	defer fallback.Stop()

	recovery := time.NewTicker(staleRecoveryInterval)
	defer recovery.Stop()

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-drainSignal:
			slog.Debug("outbox drain signaled by LISTEN")
		case <-fallback.C:
			slog.Debug("outbox drain fallback poll")
		case <-recovery.C:
			if count, err := d.pool.RecoverStaleOutboxRows(ctx); err != nil {
				slog.Warn("periodic stale outbox recovery failed", "err", err)
			} else if count > 0 {
				slog.Info("recovered stale outbox rows (periodic)", "count", count)
			}
			continue
		}

		d.drainAll(ctx)
	}
}

// drainAll keeps claiming batches until no rows remain.
func (d *Drainer) drainAll(ctx context.Context) {
	for {
		rows, err := d.pool.ClaimOutboxRows(ctx, drainClaimLimit)
		if err != nil {
			slog.Error("failed to claim outbox rows", "err", err)
			return
		}
		if len(rows) == 0 {
			return
		}

		slog.Info("claimed outbox rows", "count", len(rows))

		for i := range rows {
			d.processRow(ctx, &rows[i])
		}
	}
}

func (d *Drainer) processRow(ctx context.Context, row *db.OutboxRow) {
	switch row.PayloadType {
	case "message":
		d.handleMessage(ctx, row)
	case "chat_metadata":
		d.handleChatMetadata(ctx, row)
	case "group_registration":
		d.handleGroupRegistration(ctx, row)
	case "task":
		d.handleTask(ctx, row)
	default:
		slog.Error("unknown payload_type", "id", row.ID, "type", row.PayloadType)
		d.pool.MarkOutboxFailed(ctx, row.ID, "unknown payload_type: "+row.PayloadType)
	}
}

func (d *Drainer) handleMessage(ctx context.Context, row *db.OutboxRow) {
	var msg db.Message
	if err := json.Unmarshal(row.Payload, &msg); err != nil {
		slog.Error("permanent: failed to deserialize message payload", "id", row.ID, "err", err)
		d.pool.MarkOutboxFailed(ctx, row.ID, err.Error())
		return
	}

	chatJID := msg.ChatJID
	if err := d.pool.StoreMessage(ctx, &msg); err != nil {
		slog.Warn("transient error storing message", "id", row.ID, "err", err)
		d.pool.MarkOutboxRetry(ctx, row.ID, err.Error())
		return
	}

	if err := d.pool.MarkOutboxDelivered(ctx, row.ID); err != nil {
		slog.Error("failed to mark outbox delivered", "id", row.ID, "err", err)
	}

	// Signal the queue to check this group for pending messages
	d.queue.Enqueue(chatJID)
}

type chatMetadataPayload struct {
	JID       string  `json:"jid"`
	Timestamp string  `json:"timestamp"`
	Name      *string `json:"name"`
	Channel   *string `json:"channel"`
	IsGroup   *bool   `json:"is_group"`
}

func (d *Drainer) handleChatMetadata(ctx context.Context, row *db.OutboxRow) {
	var meta chatMetadataPayload
	if err := json.Unmarshal(row.Payload, &meta); err != nil {
		slog.Error("permanent: failed to deserialize chat_metadata payload", "id", row.ID, "err", err)
		d.pool.MarkOutboxFailed(ctx, row.ID, err.Error())
		return
	}

	if err := d.pool.StoreChatMetadata(ctx, meta.JID, meta.Timestamp, meta.Name, meta.Channel, meta.IsGroup); err != nil {
		slog.Warn("transient error storing chat metadata", "id", row.ID, "err", err)
		d.pool.MarkOutboxRetry(ctx, row.ID, err.Error())
		return
	}

	if err := d.pool.MarkOutboxDelivered(ctx, row.ID); err != nil {
		slog.Error("failed to mark outbox delivered", "id", row.ID, "err", err)
	}
}

func (d *Drainer) handleGroupRegistration(ctx context.Context, row *db.OutboxRow) {
	var group db.RegisteredGroup
	if err := json.Unmarshal(row.Payload, &group); err != nil {
		slog.Error("permanent: failed to deserialize group_registration payload", "id", row.ID, "err", err)
		d.pool.MarkOutboxFailed(ctx, row.ID, err.Error())
		return
	}

	if err := d.pool.SetRegisteredGroup(ctx, &group); err != nil {
		slog.Warn("transient error storing group registration", "id", row.ID, "err", err)
		d.pool.MarkOutboxRetry(ctx, row.ID, err.Error())
		return
	}

	if err := d.pool.MarkOutboxDelivered(ctx, row.ID); err != nil {
		slog.Error("failed to mark outbox delivered", "id", row.ID, "err", err)
	}
}

func (d *Drainer) handleTask(ctx context.Context, row *db.OutboxRow) {
	var task db.ScheduledTask
	if err := json.Unmarshal(row.Payload, &task); err != nil {
		slog.Error("permanent: failed to deserialize task payload", "id", row.ID, "err", err)
		d.pool.MarkOutboxFailed(ctx, row.ID, err.Error())
		return
	}

	if err := d.pool.CreateTask(ctx, &task); err != nil {
		slog.Warn("transient error storing task", "id", row.ID, "err", err)
		d.pool.MarkOutboxRetry(ctx, row.ID, err.Error())
		return
	}

	if err := d.pool.MarkOutboxDelivered(ctx, row.ID); err != nil {
		slog.Error("failed to mark outbox delivered", "id", row.ID, "err", err)
	}
}

// RunCleanup periodically deletes old delivered outbox rows.
// Blocks until ctx is cancelled.
func RunCleanup(ctx context.Context, pool *db.Pool) error {
	ticker := time.NewTicker(cleanupInterval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-ticker.C:
			count, err := pool.CleanupOutbox(ctx, cleanupRetentionDays)
			if err != nil {
				slog.Warn("outbox cleanup failed", "err", err)
			} else if count > 0 {
				slog.Info("outbox cleanup", "deleted", count)
			}
		}
	}
}
