package db

import (
	"context"
	"fmt"
	"log/slog"
	"sync"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

// Pool is a single-connection Postgres wrapper that reconnects on failure.
// Mirrors the Rust PgPool design: one connection behind a mutex, auto-reconnect.
type Pool struct {
	dsn  string
	mu   sync.Mutex
	conn *pgx.Conn
}

func NewPool(dsn string) *Pool {
	return &Pool{dsn: dsn}
}

func (p *Pool) Connect(ctx context.Context) error {
	p.mu.Lock()
	defer p.mu.Unlock()

	conn, err := pgx.Connect(ctx, p.dsn)
	if err != nil {
		return fmt.Errorf("connect postgres: %w", err)
	}
	if err := ensureSchema(ctx, conn); err != nil {
		conn.Close(ctx)
		return fmt.Errorf("ensure schema: %w", err)
	}
	p.conn = conn
	slog.Info("postgres connected and schema ensured")
	return nil
}

func (p *Pool) Close(ctx context.Context) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.conn != nil {
		p.conn.Close(ctx)
		p.conn = nil
	}
}

// conn returns the active connection, reconnecting if needed.
func (p *Pool) getConn(ctx context.Context) (*pgx.Conn, error) {
	p.mu.Lock()
	defer p.mu.Unlock()

	if p.conn != nil && !p.conn.IsClosed() {
		return p.conn, nil
	}

	// Reconnect
	conn, err := pgx.Connect(ctx, p.dsn)
	if err != nil {
		return nil, fmt.Errorf("reconnect postgres: %w", err)
	}
	p.conn = conn
	slog.Info("postgres reconnected")
	return p.conn, nil
}

// evict drops the current connection so the next call reconnects.
func (p *Pool) evict(ctx context.Context) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.conn != nil {
		p.conn.Close(ctx)
		p.conn = nil
	}
}

// Exec runs a query that doesn't return rows.
func (p *Pool) Exec(ctx context.Context, sql string, args ...any) (pgconn.CommandTag, error) {
	conn, err := p.getConn(ctx)
	if err != nil {
		return pgconn.CommandTag{}, err
	}
	tag, err := conn.Exec(ctx, sql, args...)
	if err != nil && isConnErr(err) {
		p.evict(ctx)
	}
	return tag, err
}

// Query runs a query that returns rows.
func (p *Pool) Query(ctx context.Context, sql string, args ...any) (pgx.Rows, error) {
	conn, err := p.getConn(ctx)
	if err != nil {
		return nil, err
	}
	rows, err := conn.Query(ctx, sql, args...)
	if err != nil && isConnErr(err) {
		p.evict(ctx)
	}
	return rows, err
}

// QueryRow runs a query that returns a single row.
func (p *Pool) QueryRow(ctx context.Context, sql string, args ...any) pgx.Row {
	conn, err := p.getConn(ctx)
	if err != nil {
		return &errRow{err: err}
	}
	return conn.QueryRow(ctx, sql, args...)
}

// Begin starts a transaction.
func (p *Pool) Begin(ctx context.Context) (pgx.Tx, error) {
	conn, err := p.getConn(ctx)
	if err != nil {
		return nil, err
	}
	return conn.Begin(ctx)
}

func isConnErr(err error) bool {
	if pgErr, ok := err.(*pgconn.PgError); ok {
		// Connection-class errors start with "08"
		return len(pgErr.Code) >= 2 && pgErr.Code[:2] == "08"
	}
	// pgx returns specific error types for closed connections
	return pgx.ErrTxClosed == err
}

// errRow implements pgx.Row for returning errors from QueryRow when connection fails.
type errRow struct {
	err error
}

func (r *errRow) Scan(_ ...any) error { return r.err }
