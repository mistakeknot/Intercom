// Package scheduler runs due scheduled tasks on a poll interval.
package scheduler

import (
	"context"
	"log/slog"
	"time"

	"github.com/mistakeknot/intercom/internal/db"
	"github.com/robfig/cron/v3"
)

// TaskRunner is called to execute a scheduled task. Returns the result text.
type TaskRunner func(ctx context.Context, task db.ScheduledTask) (string, error)

// Scheduler polls for due tasks and dispatches them.
type Scheduler struct {
	pool     *db.Pool
	runner   TaskRunner
	interval time.Duration
}

func New(pool *db.Pool, runner TaskRunner, intervalMs int) *Scheduler {
	return &Scheduler{
		pool:     pool,
		runner:   runner,
		interval: time.Duration(intervalMs) * time.Millisecond,
	}
}

// Run starts the scheduler loop. Blocks until ctx is cancelled.
func (s *Scheduler) Run(ctx context.Context) error {
	ticker := time.NewTicker(s.interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-ticker.C:
			s.tick(ctx)
		}
	}
}

func (s *Scheduler) tick(ctx context.Context) {
	tasks, err := s.pool.ClaimDueTasks(ctx, 5)
	if err != nil {
		slog.Error("claim due tasks", "err", err)
		return
	}

	for _, task := range tasks {
		s.executeTask(ctx, task)
	}
}

func (s *Scheduler) executeTask(ctx context.Context, task db.ScheduledTask) {
	start := time.Now()
	result, err := s.runner(ctx, task)
	duration := time.Since(start)

	status := "success"
	var errMsg *string
	if err != nil {
		status = "error"
		e := err.Error()
		errMsg = &e
		slog.Error("task execution failed", "task_id", task.ID, "err", err)
	} else {
		slog.Info("task executed", "task_id", task.ID, "duration", duration)
	}

	log := &db.TaskRunLog{
		TaskID:     task.ID,
		RunAt:      start.UTC().Format(time.RFC3339Nano),
		DurationMs: duration.Milliseconds(),
		Status:     status,
		Result:     &result,
		Error:      errMsg,
	}

	// Compute next run (for recurring tasks)
	var nextRun *string
	if task.ScheduleType == "cron" || task.ScheduleType == "interval" {
		// For now, use a simple next-run calculation.
		// A full cron parser would be added in a later iteration.
		next := computeNextRun(task.ScheduleType, task.ScheduleValue)
		if next != "" {
			nextRun = &next
		}
	}

	if err := s.pool.LogAndUpdateTask(ctx, log, task.ID, nextRun, result); err != nil {
		slog.Error("log_and_update_task", "task_id", task.ID, "err", err)
	}
}

func computeNextRun(scheduleType, scheduleValue string) string {
	switch scheduleType {
	case "interval":
		// Parse duration string (e.g. "1h", "30m")
		d, err := time.ParseDuration(scheduleValue)
		if err != nil {
			slog.Warn("invalid interval", "value", scheduleValue, "err", err)
			return ""
		}
		return time.Now().Add(d).UTC().Format(time.RFC3339Nano)
	case "cron":
		parser := cron.NewParser(cron.Minute | cron.Hour | cron.Dom | cron.Month | cron.Dow)
		sched, err := parser.Parse(scheduleValue)
		if err != nil {
			slog.Warn("invalid cron expression", "value", scheduleValue, "err", err)
			return ""
		}
		return sched.Next(time.Now()).UTC().Format(time.RFC3339Nano)
	default:
		return "" // one-shot task, no next run
	}
}
