package subprocess

import (
	"encoding/json"
	"os"
	"strings"
	"testing"
)

func TestWriteMCPConfig(t *testing.T) {
	path, err := WriteMCPConfig(MCPServerConfig{
		IntercomdBinary: "/usr/local/bin/intercomd",
		ConfigPath:      "config/intercom.toml",
	})
	if err != nil {
		t.Fatalf("WriteMCPConfig: %v", err)
	}
	defer os.Remove(path)

	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read: %v", err)
	}

	var config mcpConfigJSON
	if err := json.Unmarshal(data, &config); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	server, ok := config.MCPServers["intercom"]
	if !ok {
		t.Fatal("missing 'intercom' server entry")
	}
	if server.Command != "/usr/local/bin/intercomd" {
		t.Errorf("command = %q, want /usr/local/bin/intercomd", server.Command)
	}
	if len(server.Args) != 3 {
		t.Fatalf("args len = %d, want 3", len(server.Args))
	}
	if server.Args[0] != "mcp-server" {
		t.Errorf("args[0] = %q, want mcp-server", server.Args[0])
	}
	if server.Args[1] != "--config" {
		t.Errorf("args[1] = %q, want --config", server.Args[1])
	}
	if server.Args[2] != "config/intercom.toml" {
		t.Errorf("args[2] = %q, want config/intercom.toml", server.Args[2])
	}
}

func TestWriteMCPConfigNoConfigPath(t *testing.T) {
	path, err := WriteMCPConfig(MCPServerConfig{
		IntercomdBinary: "/usr/local/bin/intercomd",
	})
	if err != nil {
		t.Fatalf("WriteMCPConfig: %v", err)
	}
	defer os.Remove(path)

	data, _ := os.ReadFile(path)
	var config mcpConfigJSON
	json.Unmarshal(data, &config)

	server := config.MCPServers["intercom"]
	if len(server.Args) != 1 {
		t.Errorf("args len = %d, want 1 (just mcp-server)", len(server.Args))
	}
}

func TestWriteMCPConfigEmptyBinary(t *testing.T) {
	_, err := WriteMCPConfig(MCPServerConfig{})
	if err == nil {
		t.Error("expected error for empty IntercomdBinary")
	}
}

func TestWriteMCPConfigTempFileCleanup(t *testing.T) {
	path, err := WriteMCPConfig(MCPServerConfig{
		IntercomdBinary: "/usr/local/bin/intercomd",
	})
	if err != nil {
		t.Fatalf("WriteMCPConfig: %v", err)
	}

	// File should exist
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("temp file should exist: %v", err)
	}

	// File should have intercom-mcp prefix
	if !strings.Contains(path, "intercom-mcp") {
		t.Errorf("temp file path %q should contain 'intercom-mcp'", path)
	}

	os.Remove(path)

	// After removal, should not exist
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Error("temp file should be gone after cleanup")
	}
}

func TestBuildArgsWithMCPConfig(t *testing.T) {
	args, err := buildArgs(StartConfig{
		Runtime:   "claude",
		Model:     "claude-opus-4-6",
		Prompt:    "hello",
		MCPConfig: "/tmp/test-mcp.json",
	})
	if err != nil {
		t.Fatalf("buildArgs: %v", err)
	}

	// Should contain --mcp-config
	found := false
	for i, a := range args {
		if a == "--mcp-config" && i+1 < len(args) && args[i+1] == "/tmp/test-mcp.json" {
			found = true
			break
		}
	}
	if !found {
		t.Errorf("args %v should contain --mcp-config /tmp/test-mcp.json", args)
	}
}

func TestBuildArgsWithoutMCPConfig(t *testing.T) {
	args, err := buildArgs(StartConfig{
		Runtime: "claude",
		Model:   "claude-opus-4-6",
		Prompt:  "hello",
	})
	if err != nil {
		t.Fatalf("buildArgs: %v", err)
	}

	for _, a := range args {
		if a == "--mcp-config" {
			t.Error("args should not contain --mcp-config when empty")
		}
	}
}

func TestResolveIntercomdBinary(t *testing.T) {
	// Without INTERCOMD_BINARY env, should return "intercomd" fallback
	t.Setenv("INTERCOMD_BINARY", "")
	result := ResolveIntercomdBinary()
	// Result is either the current binary path (if named intercomd) or "intercomd"
	if result == "" {
		t.Error("should return non-empty binary path")
	}
}

func TestResolveIntercomdBinaryEnvOverride(t *testing.T) {
	t.Setenv("INTERCOMD_BINARY", "/custom/path/intercomd")
	result := ResolveIntercomdBinary()
	if result != "/custom/path/intercomd" {
		t.Errorf("got %q, want /custom/path/intercomd", result)
	}
}
