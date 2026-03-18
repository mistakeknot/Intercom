package db

import (
	"context"
	"log/slog"
	"time"

	"github.com/jackc/pgx/v5"
)

const OutboxChannel = "intercom_outbox"

// ListenLoop maintains a dedicated Postgres connection for LISTEN on the
// outbox channel. On each notification, it sends to drainSignal.
// Reconnects with exponential backoff on connection loss.
// Blocks until ctx is cancelled.
func ListenLoop(ctx context.Context, dsn string, drainSignal chan<- struct{}) error {
	backoff := 1 * time.Second
	maxBackoff := 30 * time.Second

	for {
		if ctx.Err() != nil {
			return ctx.Err()
		}

		slog.Info("LISTEN loop connecting", "dsn", redactDSN(dsn))

		conn, err := pgx.Connect(ctx, dsn)
		if err != nil {
			slog.Warn("LISTEN connect failed, retrying", "err", err, "backoff", backoff)
			select {
			case <-time.After(backoff):
			case <-ctx.Done():
				return ctx.Err()
			}
			backoff = min(backoff*2, maxBackoff)
			continue
		}

		// Reset backoff on successful connect
		backoff = 1 * time.Second

		// Issue LISTEN
		_, err = conn.Exec(ctx, "LISTEN "+OutboxChannel)
		if err != nil {
			slog.Warn("LISTEN command failed", "err", err)
			conn.Close(ctx)
			continue
		}

		slog.Info("LISTEN active", "channel", OutboxChannel)

		// Signal initial drain in case rows accumulated before LISTEN
		nudge(drainSignal)

		// Wait for notifications until connection drops
		err = listenOnConn(ctx, conn, drainSignal)
		conn.Close(ctx)

		if ctx.Err() != nil {
			return ctx.Err()
		}

		slog.Warn("LISTEN reconnecting", "err", err, "backoff", backoff)
		select {
		case <-time.After(backoff):
		case <-ctx.Done():
			return ctx.Err()
		}
		backoff = min(backoff*2, maxBackoff)
	}
}

func listenOnConn(ctx context.Context, conn *pgx.Conn, drainSignal chan<- struct{}) error {
	for {
		_, err := conn.WaitForNotification(ctx)
		if err != nil {
			return err
		}
		nudge(drainSignal)
	}
}

// nudge sends a non-blocking signal on the channel.
func nudge(ch chan<- struct{}) {
	select {
	case ch <- struct{}{}:
	default:
	}
}

// redactDSN masks the password in a Postgres DSN for safe logging.
func redactDSN(dsn string) string {
	schemeEnd := indexOf(dsn, "://")
	if schemeEnd < 0 {
		return dsn
	}
	atPos := indexByte(dsn, '@')
	if atPos < 0 {
		return dsn
	}
	userinfo := dsn[schemeEnd+3 : atPos]
	colonPos := indexByte(userinfo, ':')
	if colonPos < 0 {
		return dsn
	}
	absColon := schemeEnd + 3 + colonPos
	return dsn[:absColon+1] + "***" + dsn[atPos:]
}

func indexOf(s, substr string) int {
	for i := 0; i <= len(s)-len(substr); i++ {
		if s[i:i+len(substr)] == substr {
			return i
		}
	}
	return -1
}

func indexByte(s string, b byte) int {
	for i := 0; i < len(s); i++ {
		if s[i] == b {
			return i
		}
	}
	return -1
}
