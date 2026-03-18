package db

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/jackc/pgx/v5"
)

// ValidateGroupFolder rejects values that could escape the IPC directory tree.
func ValidateGroupFolder(folder string) error {
	if folder == "" || strings.Contains(folder, "..") || strings.Contains(folder, "/") || strings.Contains(folder, "\\") {
		return fmt.Errorf("invalid group_folder: %s", folder)
	}
	return nil
}

func (p *Pool) GetRegisteredGroup(ctx context.Context, jid string) (*RegisteredGroup, error) {
	row := p.QueryRow(ctx, `SELECT * FROM registered_groups WHERE jid = $1`, jid)
	g, err := scanRegisteredGroup(row)
	if err == pgx.ErrNoRows {
		return nil, nil
	}
	return g, err
}

func (p *Pool) SetRegisteredGroup(ctx context.Context, g *RegisteredGroup) error {
	requiresTrigger := true
	if g.RequiresTrigger != nil {
		requiresTrigger = *g.RequiresTrigger
	}
	var configJSON any
	if g.ContainerConfig != nil {
		configJSON = g.ContainerConfig
	}
	_, err := p.Exec(ctx, `
		INSERT INTO registered_groups
		  (jid, name, folder, trigger_pattern, added_at, container_config, requires_trigger, runtime, model)
		VALUES ($1, $2, $3, $4, $5::text::timestamptz, $6, $7, $8, $9)
		ON CONFLICT (jid) DO UPDATE SET
		  name = EXCLUDED.name,
		  folder = EXCLUDED.folder,
		  trigger_pattern = EXCLUDED.trigger_pattern,
		  container_config = EXCLUDED.container_config,
		  requires_trigger = EXCLUDED.requires_trigger,
		  runtime = EXCLUDED.runtime,
		  model = EXCLUDED.model`,
		g.JID, g.Name, g.Folder, g.TriggerPattern, g.AddedAt,
		configJSON, requiresTrigger, g.Runtime, g.Model,
	)
	return err
}

func (p *Pool) GetAllRegisteredGroups(ctx context.Context) (map[string]RegisteredGroup, error) {
	rows, err := p.Query(ctx, `SELECT * FROM registered_groups`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	result := make(map[string]RegisteredGroup)
	for rows.Next() {
		g, err := scanRegisteredGroupRow(rows)
		if err != nil {
			return nil, err
		}
		result[g.JID] = *g
	}
	return result, rows.Err()
}

func scanRegisteredGroup(row pgx.Row) (*RegisteredGroup, error) {
	var g RegisteredGroup
	var addedAt time.Time
	var configJSON []byte
	if err := row.Scan(
		&g.JID, &g.Name, &g.Folder, &g.TriggerPattern, &addedAt,
		&configJSON, &g.RequiresTrigger, &g.Runtime, &g.Model,
	); err != nil {
		return nil, err
	}
	g.AddedAt = addedAt.Format(time.RFC3339Nano)
	if configJSON != nil {
		g.ContainerConfig = json.RawMessage(configJSON)
	}
	return &g, nil
}

func scanRegisteredGroupRow(rows pgx.Rows) (*RegisteredGroup, error) {
	var g RegisteredGroup
	var addedAt time.Time
	var configJSON []byte
	if err := rows.Scan(
		&g.JID, &g.Name, &g.Folder, &g.TriggerPattern, &addedAt,
		&configJSON, &g.RequiresTrigger, &g.Runtime, &g.Model,
	); err != nil {
		return nil, err
	}
	g.AddedAt = addedAt.Format(time.RFC3339Nano)
	if configJSON != nil {
		g.ContainerConfig = json.RawMessage(configJSON)
	}
	return &g, nil
}
