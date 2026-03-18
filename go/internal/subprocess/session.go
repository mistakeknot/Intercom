package subprocess

import (
	"context"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"sync"
)

// SessionStore abstracts Postgres session persistence.
type SessionStore interface {
	SetSession(ctx context.Context, groupFolder, sessionID string) error
	DeleteSession(ctx context.Context, groupFolder string) error
}

// SessionTracker manages per-group session IDs and their on-disk JSONL files.
// It mirrors the Rust process_group.rs behavior: before spawning a subprocess,
// check if the session JSONL exceeds session_max_bytes and auto-reset if so.
type SessionTracker struct {
	mu       sync.RWMutex
	sessions map[string]string // group folder → session ID

	dataDir         string // root data directory (contains sessions/ subdir)
	sessionMaxBytes int64  // max JSONL file size before auto-reset
}

func NewSessionTracker(dataDir string, sessionMaxBytes int64) *SessionTracker {
	return &SessionTracker{
		sessions:        make(map[string]string),
		dataDir:         dataDir,
		sessionMaxBytes: sessionMaxBytes,
	}
}

// LoadAll populates the in-memory map from an external source (typically Postgres).
func (st *SessionTracker) LoadAll(sessions map[string]string) {
	st.mu.Lock()
	defer st.mu.Unlock()
	for k, v := range sessions {
		st.sessions[k] = v
	}
}

// Get returns the session ID for a group, or empty string if none.
func (st *SessionTracker) Get(groupFolder string) string {
	st.mu.RLock()
	defer st.mu.RUnlock()
	return st.sessions[groupFolder]
}

// Set stores a session ID for a group.
func (st *SessionTracker) Set(groupFolder, sessionID string) {
	st.mu.Lock()
	defer st.mu.Unlock()
	st.sessions[groupFolder] = sessionID
}

// Delete removes a session from the in-memory map.
func (st *SessionTracker) Delete(groupFolder string) {
	st.mu.Lock()
	defer st.mu.Unlock()
	delete(st.sessions, groupFolder)
}

// SessionJSONLPath returns the path to a session's JSONL file.
// Matches Rust: {data_dir}/sessions/{group}/.claude/projects/-workspace-group/{sid}.jsonl
func (st *SessionTracker) SessionJSONLPath(groupFolder, sessionID string) string {
	return filepath.Join(
		st.dataDir, "sessions", groupFolder,
		".claude", "projects", "-workspace-group",
		sessionID+".jsonl",
	)
}

// sessionDir returns the session subdirectory that claude creates alongside the JSONL.
// Matches Rust: parent_of_jsonl / session_id /
func (st *SessionTracker) sessionDir(groupFolder, sessionID string) string {
	jsonlPath := st.SessionJSONLPath(groupFolder, sessionID)
	return filepath.Join(filepath.Dir(jsonlPath), sessionID)
}

// CheckAndReset checks if the current session's JSONL file exceeds the size limit.
// If it does, it deletes the JSONL file and session directory, clears from memory,
// and deletes from the store (Postgres). Returns true if a reset occurred.
//
// This prevents bloated session context from causing result timeouts — the same
// guard as Rust's "5a. Session size guard" in process_group.rs.
func (st *SessionTracker) CheckAndReset(ctx context.Context, groupFolder string, store SessionStore) (bool, error) {
	sid := st.Get(groupFolder)
	if sid == "" {
		return false, nil
	}

	jsonlPath := st.SessionJSONLPath(groupFolder, sid)
	info, err := os.Stat(jsonlPath)
	if err != nil {
		// File doesn't exist — nothing to reset
		return false, nil
	}

	if info.Size() <= st.sessionMaxBytes {
		return false, nil
	}

	slog.Warn("session file exceeds size limit — auto-resetting",
		"group", groupFolder,
		"session_id", sid,
		"file_bytes", info.Size(),
		"max_bytes", st.sessionMaxBytes,
	)

	// Delete the session JSONL file
	if err := os.Remove(jsonlPath); err != nil && !os.IsNotExist(err) {
		slog.Warn("failed to remove session JSONL", "path", jsonlPath, "err", err)
	}

	// Delete the session directory (claude creates a dir alongside the JSONL)
	sessionDir := st.sessionDir(groupFolder, sid)
	if err := os.RemoveAll(sessionDir); err != nil {
		slog.Warn("failed to remove session dir", "path", sessionDir, "err", err)
	}

	// Clear from memory
	st.Delete(groupFolder)

	// Clear from Postgres
	if store != nil {
		if err := store.DeleteSession(ctx, groupFolder); err != nil {
			return true, fmt.Errorf("delete session from store: %w", err)
		}
	}

	return true, nil
}

// PersistSession stores a new session ID both in memory and in the store.
func (st *SessionTracker) PersistSession(ctx context.Context, groupFolder, sessionID string, store SessionStore) error {
	st.Set(groupFolder, sessionID)
	if store != nil {
		return store.SetSession(ctx, groupFolder, sessionID)
	}
	return nil
}

// ClearSession removes a session from memory and store (used after result timeout).
func (st *SessionTracker) ClearSession(ctx context.Context, groupFolder string, store SessionStore) error {
	st.Delete(groupFolder)
	if store != nil {
		return store.DeleteSession(ctx, groupFolder)
	}
	return nil
}
