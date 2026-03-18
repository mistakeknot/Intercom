package db

import (
	"context"
	"encoding/json"
	"fmt"
	"time"

	"github.com/jackc/pgx/v5"
)

// ClaimOutboxRows atomically claims up to limit pending rows.
// Uses SELECT FOR UPDATE SKIP LOCKED to prevent double-processing.
func (p *Pool) ClaimOutboxRows(ctx context.Context, limit int) ([]OutboxRow, error) {
	tx, err := p.Begin(ctx)
	if err != nil {
		return nil, fmt.Errorf("begin claim tx: %w", err)
	}
	defer tx.Rollback(ctx)

	rows, err := tx.Query(ctx, `
		UPDATE message_outbox
		SET status = 'processing', attempts = attempts + 1
		WHERE id IN (
		  SELECT id FROM message_outbox
		  WHERE status = 'pending' AND attempts < 5
		  ORDER BY created_at
		  FOR UPDATE SKIP LOCKED
		  LIMIT $1
		)
		RETURNING id, chat_jid, payload_type, payload, status, created_at, attempts`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	result, err := collectOutboxRows(rows)
	if err != nil {
		return nil, err
	}
	if err := tx.Commit(ctx); err != nil {
		return nil, fmt.Errorf("commit claim tx: %w", err)
	}
	return result, nil
}

func (p *Pool) MarkOutboxDelivered(ctx context.Context, id int64) error {
	_, err := p.Exec(ctx,
		`UPDATE message_outbox SET status = 'delivered', delivered_at = now() WHERE id = $1`, id)
	return err
}

func (p *Pool) MarkOutboxFailed(ctx context.Context, id int64, errMsg string) error {
	_, err := p.Exec(ctx,
		`UPDATE message_outbox SET status = 'failed', last_error = $2 WHERE id = $1`, id, errMsg)
	return err
}

// MarkOutboxRetry resets a row to pending for retry. If max attempts reached, marks failed.
// Fires NOTIFY so the drain loop picks up retried rows immediately.
func (p *Pool) MarkOutboxRetry(ctx context.Context, id int64, errMsg string) error {
	_, err := p.Exec(ctx, `
		UPDATE message_outbox SET
		  status = CASE WHEN attempts >= 5 THEN 'failed' ELSE 'pending' END,
		  last_error = $2
		WHERE id = $1`, id, errMsg)
	if err != nil {
		return err
	}
	// Fire NOTIFY so drain loop picks up immediately
	_, _ = p.Exec(ctx, `SELECT pg_notify('intercom_outbox', '')`)
	return nil
}

// RecoverStaleOutboxRows resets processing rows older than 5 minutes back to pending.
func (p *Pool) RecoverStaleOutboxRows(ctx context.Context) (int64, error) {
	tag, err := p.Exec(ctx, `
		UPDATE message_outbox SET status = 'pending'
		WHERE status = 'processing'
		AND created_at < now() - interval '5 minutes'`)
	if err != nil {
		return 0, err
	}
	return tag.RowsAffected(), nil
}

func (p *Pool) CleanupOutbox(ctx context.Context, olderThanDays int) (int64, error) {
	if olderThanDays <= 0 {
		return 0, fmt.Errorf("cleanup_outbox: older_than_days out of range: %d", olderThanDays)
	}
	tag, err := p.Exec(ctx, `
		DELETE FROM message_outbox
		WHERE status = 'delivered' AND delivered_at < now() - make_interval(days => $1::int)`,
		olderThanDays)
	if err != nil {
		return 0, err
	}
	return tag.RowsAffected(), nil
}

func (p *Pool) OutboxStats(ctx context.Context) (*OutboxStats, error) {
	var s OutboxStats
	err := p.QueryRow(ctx, `
		SELECT
		  COUNT(*) FILTER (WHERE status = 'pending') AS pending,
		  COUNT(*) FILTER (WHERE status = 'processing') AS processing,
		  COUNT(*) FILTER (WHERE status = 'failed') AS failed
		FROM message_outbox`).Scan(&s.Pending, &s.Processing, &s.Failed)
	return &s, err
}

// Scheduled task operations

func (p *Pool) CreateTask(ctx context.Context, task *ScheduledTask) error {
	if err := ValidateGroupFolder(task.GroupFolder); err != nil {
		return err
	}
	_, err := p.Exec(ctx, `
		INSERT INTO scheduled_tasks
		  (id, group_folder, chat_jid, prompt, schedule_type, schedule_value, context_mode, next_run, status, created_at)
		VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::timestamptz, $9, $10::text::timestamptz)`,
		task.ID, task.GroupFolder, task.ChatJID, task.Prompt,
		task.ScheduleType, task.ScheduleValue, task.ContextMode,
		task.NextRun, task.Status, task.CreatedAt,
	)
	return err
}

func (p *Pool) GetTaskByID(ctx context.Context, id string) (*ScheduledTask, error) {
	row := p.QueryRow(ctx, `SELECT * FROM scheduled_tasks WHERE id = $1`, id)
	t, err := scanTask(row)
	if err == pgx.ErrNoRows {
		return nil, nil
	}
	return t, err
}

func (p *Pool) GetTasksForGroup(ctx context.Context, groupFolder string) ([]ScheduledTask, error) {
	rows, err := p.Query(ctx,
		`SELECT * FROM scheduled_tasks WHERE group_folder = $1 ORDER BY created_at DESC`, groupFolder)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return collectTasks(rows)
}

func (p *Pool) GetAllTasks(ctx context.Context) ([]ScheduledTask, error) {
	rows, err := p.Query(ctx, `SELECT * FROM scheduled_tasks ORDER BY created_at DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return collectTasks(rows)
}

// ClaimDueTasks atomically claims up to limit due tasks by setting status='running'.
func (p *Pool) ClaimDueTasks(ctx context.Context, limit int) ([]ScheduledTask, error) {
	rows, err := p.Query(ctx, `
		UPDATE scheduled_tasks
		SET status = 'running'
		WHERE id IN (
		  SELECT id FROM scheduled_tasks
		  WHERE status = 'active' AND next_run IS NOT NULL AND next_run <= now()
		  ORDER BY next_run
		  FOR UPDATE SKIP LOCKED
		  LIMIT $1
		)
		RETURNING *`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return collectTasks(rows)
}

func (p *Pool) DeleteTask(ctx context.Context, id string) error {
	_, err := p.Exec(ctx, `DELETE FROM scheduled_tasks WHERE id = $1`, id)
	return err
}

// LogAndUpdateTask atomically logs a run and updates the task in a single transaction.
func (p *Pool) LogAndUpdateTask(ctx context.Context, log *TaskRunLog, taskID string, nextRun *string, lastResult string) error {
	tx, err := p.Begin(ctx)
	if err != nil {
		return fmt.Errorf("begin log_and_update tx: %w", err)
	}
	defer tx.Rollback(ctx)

	_, err = tx.Exec(ctx, `
		INSERT INTO task_run_logs (task_id, run_at, duration_ms, status, result, error)
		VALUES ($1, $2::text::timestamptz, $3, $4, $5, $6)`,
		log.TaskID, log.RunAt, log.DurationMs, log.Status, log.Result, log.Error)
	if err != nil {
		return err
	}

	now := time.Now().UTC().Format(time.RFC3339Nano)
	_, err = tx.Exec(ctx, `
		UPDATE scheduled_tasks
		SET next_run = $1::text::timestamptz, last_run = $2::text::timestamptz,
		    last_result = $3,
		    status = CASE WHEN $1 IS NULL THEN 'completed' ELSE 'active' END
		WHERE id = $4`,
		nextRun, now, lastResult, taskID)
	if err != nil {
		return err
	}

	return tx.Commit(ctx)
}

func scanTask(row pgx.Row) (*ScheduledTask, error) {
	var t ScheduledTask
	var createdAt time.Time
	var nextRun, lastRun *time.Time
	if err := row.Scan(
		&t.ID, &t.GroupFolder, &t.ChatJID, &t.Prompt,
		&t.ScheduleType, &t.ScheduleValue, &t.ContextMode,
		&nextRun, &lastRun, &t.LastResult, &t.Status, &createdAt,
	); err != nil {
		return nil, err
	}
	t.CreatedAt = createdAt.Format(time.RFC3339Nano)
	if nextRun != nil {
		s := nextRun.Format(time.RFC3339Nano)
		t.NextRun = &s
	}
	if lastRun != nil {
		s := lastRun.Format(time.RFC3339Nano)
		t.LastRun = &s
	}
	return &t, nil
}

func collectTasks(rows pgx.Rows) ([]ScheduledTask, error) {
	var tasks []ScheduledTask
	for rows.Next() {
		var t ScheduledTask
		var createdAt time.Time
		var nextRun, lastRun *time.Time
		if err := rows.Scan(
			&t.ID, &t.GroupFolder, &t.ChatJID, &t.Prompt,
			&t.ScheduleType, &t.ScheduleValue, &t.ContextMode,
			&nextRun, &lastRun, &t.LastResult, &t.Status, &createdAt,
		); err != nil {
			return nil, err
		}
		t.CreatedAt = createdAt.Format(time.RFC3339Nano)
		if nextRun != nil {
			s := nextRun.Format(time.RFC3339Nano)
			t.NextRun = &s
		}
		if lastRun != nil {
			s := lastRun.Format(time.RFC3339Nano)
			t.LastRun = &s
		}
		tasks = append(tasks, t)
	}
	return tasks, rows.Err()
}

func collectOutboxRows(rows pgx.Rows) ([]OutboxRow, error) {
	var result []OutboxRow
	for rows.Next() {
		var r OutboxRow
		var payload json.RawMessage
		var createdAt time.Time
		if err := rows.Scan(&r.ID, &r.ChatJID, &r.PayloadType, &payload, &r.Status, &createdAt, &r.Attempts); err != nil {
			return nil, err
		}
		r.Payload = payload
		r.CreatedAt = createdAt.Format(time.RFC3339Nano)
		result = append(result, r)
	}
	return result, rows.Err()
}
