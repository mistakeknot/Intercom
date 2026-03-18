package db

import (
	"context"

	"github.com/jackc/pgx/v5"
)

func (p *Pool) GetSession(ctx context.Context, groupFolder string) (string, error) {
	var sessionID string
	err := p.QueryRow(ctx, `SELECT session_id FROM sessions WHERE group_folder = $1`, groupFolder).Scan(&sessionID)
	if err == pgx.ErrNoRows {
		return "", nil
	}
	return sessionID, err
}

func (p *Pool) SetSession(ctx context.Context, groupFolder, sessionID string) error {
	_, err := p.Exec(ctx, `
		INSERT INTO sessions (group_folder, session_id) VALUES ($1, $2)
		ON CONFLICT (group_folder) DO UPDATE SET session_id = EXCLUDED.session_id`,
		groupFolder, sessionID)
	return err
}

func (p *Pool) GetAllSessions(ctx context.Context) (map[string]string, error) {
	rows, err := p.Query(ctx, `SELECT group_folder, session_id FROM sessions`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	result := make(map[string]string)
	for rows.Next() {
		var folder, sid string
		if err := rows.Scan(&folder, &sid); err != nil {
			return nil, err
		}
		result[folder] = sid
	}
	return result, rows.Err()
}

func (p *Pool) DeleteSession(ctx context.Context, groupFolder string) error {
	_, err := p.Exec(ctx, `DELETE FROM sessions WHERE group_folder = $1`, groupFolder)
	return err
}

// Router state

func (p *Pool) GetRouterState(ctx context.Context, key string) (string, error) {
	var value string
	err := p.QueryRow(ctx, `SELECT value FROM router_state WHERE key = $1`, key).Scan(&value)
	if err == pgx.ErrNoRows {
		return "", nil
	}
	return value, err
}

func (p *Pool) SetRouterState(ctx context.Context, key, value string) error {
	_, err := p.Exec(ctx, `
		INSERT INTO router_state (key, value) VALUES ($1, $2)
		ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value`,
		key, value)
	return err
}
