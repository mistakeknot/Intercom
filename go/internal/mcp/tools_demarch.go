package mcp

import (
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"strings"
	"time"
)

// RegisterDemarchTools adds Demarch CLI tools (bd, ic) to the MCP server.
// Commands are validated against allowlists before execution.
func RegisterDemarchTools(s *Server, readAllowlist, writeAllowlist []string) {
	s.RegisterTool(Tool{
		Name:        "demarch_read",
		Description: "Run a read-only Demarch CLI command (bd list, ic run status, etc.)",
		InputSchema: json.RawMessage(`{
			"type": "object",
			"properties": {
				"command": {"type": "string", "description": "The CLI command to run (e.g. 'bd list --json')"}
			},
			"required": ["command"]
		}`),
		Handler: func(ctx context.Context, input json.RawMessage) (any, error) {
			var args struct {
				Command string `json:"command"`
			}
			if err := json.Unmarshal(input, &args); err != nil {
				return nil, err
			}
			if !isAllowed(args.Command, readAllowlist) {
				return nil, fmt.Errorf("command not in read allowlist: %s", args.Command)
			}
			return runCommand(ctx, args.Command)
		},
	})

	s.RegisterTool(Tool{
		Name:        "demarch_write",
		Description: "Run a write Demarch CLI command (bd create, ic run create, etc.)",
		InputSchema: json.RawMessage(`{
			"type": "object",
			"properties": {
				"command": {"type": "string", "description": "The CLI command to run (e.g. 'bd create --json')"}
			},
			"required": ["command"]
		}`),
		Handler: func(ctx context.Context, input json.RawMessage) (any, error) {
			var args struct {
				Command string `json:"command"`
			}
			if err := json.Unmarshal(input, &args); err != nil {
				return nil, err
			}
			if !isAllowed(args.Command, writeAllowlist) {
				return nil, fmt.Errorf("command not in write allowlist: %s", args.Command)
			}
			return runCommand(ctx, args.Command)
		},
	})
}

// isAllowed checks if a command matches any prefix in the allowlist.
func isAllowed(command string, allowlist []string) bool {
	cmd := strings.TrimSpace(command)
	for _, prefix := range allowlist {
		if strings.HasPrefix(cmd, prefix) {
			return true
		}
	}
	return false
}

func runCommand(ctx context.Context, command string) (any, error) {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()

	parts := strings.Fields(command)
	if len(parts) == 0 {
		return nil, fmt.Errorf("empty command")
	}

	cmd := exec.CommandContext(ctx, parts[0], parts[1:]...)
	out, err := cmd.Output()
	if err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			return map[string]any{
				"exit_code": exitErr.ExitCode(),
				"stderr":    string(exitErr.Stderr),
				"stdout":    string(out),
			}, nil
		}
		return nil, err
	}

	// Try to parse as JSON
	var parsed any
	if err := json.Unmarshal(out, &parsed); err == nil {
		return parsed, nil
	}
	return map[string]any{"output": string(out)}, nil
}
