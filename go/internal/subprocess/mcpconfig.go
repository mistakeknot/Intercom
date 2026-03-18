package subprocess

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// MCPServerConfig holds the parameters needed to generate a temp mcp.json
// that tells claude -p to spawn intercomd mcp-server as a stdio MCP server.
type MCPServerConfig struct {
	// IntercomdBinary is the path to the intercomd binary.
	// If empty, MCP config generation is skipped.
	IntercomdBinary string

	// ConfigPath is the path to intercom.toml (passed as --config to mcp-server).
	ConfigPath string
}

// mcpConfigJSON matches the format claude --mcp-config expects.
type mcpConfigJSON struct {
	MCPServers map[string]mcpServerEntry `json:"mcpServers"`
}

type mcpServerEntry struct {
	Command string   `json:"command"`
	Args    []string `json:"args"`
}

// WriteMCPConfig creates a temporary mcp.json file that configures claude -p
// to spawn intercomd mcp-server as a stdio MCP server.
// Returns the temp file path. Caller is responsible for cleanup.
func WriteMCPConfig(cfg MCPServerConfig) (string, error) {
	if cfg.IntercomdBinary == "" {
		return "", fmt.Errorf("IntercomdBinary is required")
	}

	args := []string{"mcp-server"}
	if cfg.ConfigPath != "" {
		args = append(args, "--config", cfg.ConfigPath)
	}

	config := mcpConfigJSON{
		MCPServers: map[string]mcpServerEntry{
			"intercom": {
				Command: cfg.IntercomdBinary,
				Args:    args,
			},
		},
	}

	data, err := json.MarshalIndent(config, "", "  ")
	if err != nil {
		return "", fmt.Errorf("marshal mcp config: %w", err)
	}

	// Write to a temp file in the system temp dir
	f, err := os.CreateTemp("", "intercom-mcp-*.json")
	if err != nil {
		return "", fmt.Errorf("create temp mcp config: %w", err)
	}

	if _, err := f.Write(data); err != nil {
		f.Close()
		os.Remove(f.Name())
		return "", fmt.Errorf("write mcp config: %w", err)
	}

	if err := f.Close(); err != nil {
		os.Remove(f.Name())
		return "", err
	}

	return f.Name(), nil
}

// ResolveIntercomdBinary finds the intercomd binary path.
// Checks: explicit path > $INTERCOMD_BINARY > os.Executable() > PATH lookup.
func ResolveIntercomdBinary() string {
	// 1. Environment variable override
	if v := os.Getenv("INTERCOMD_BINARY"); v != "" {
		return v
	}

	// 2. Current executable (if we ARE intercomd)
	if exe, err := os.Executable(); err == nil {
		base := filepath.Base(exe)
		if base == "intercomd" || base == "intercomd.exe" {
			return exe
		}
	}

	// 3. Look in PATH
	// exec.LookPath would work but we avoid importing exec here.
	// The caller (main.go) can resolve this more precisely.
	return "intercomd"
}
