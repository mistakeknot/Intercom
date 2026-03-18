package subprocess

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"sync"
	"time"
)

// Process wraps a single CLI agent subprocess (claude -p, codex exec, etc.).
type Process struct {
	cmd     *exec.Cmd
	stdin   io.WriteCloser
	stdout  io.ReadCloser
	stderr  io.ReadCloser
	done    chan struct{}
	err     error
	mu      sync.Mutex
	started time.Time
}

// Frame is a single JSON-lines output frame from the agent.
type Frame struct {
	Type    string          `json:"type"`
	Content string          `json:"content,omitempty"`
	Data    json.RawMessage `json:"data,omitempty"`
}

// StartConfig configures a subprocess launch.
type StartConfig struct {
	Runtime    string           // "claude", "gemini", "codex"
	Model      string           // e.g. "claude-opus-4-6"
	WorkDir    string           // working directory for the agent
	Prompt     string           // initial prompt
	SessionDir string           // session JSONL directory
	MCPConfig  string           // path to MCP config JSON (explicit)
	MCPServer  *MCPServerConfig // auto-generate MCP config from intercomd binary
	Env        []string         // additional env vars
	Args       []string         // additional CLI args
}

// Start launches an agent subprocess.
// If MCPServer is set and MCPConfig is empty, auto-generates a temp mcp.json
// and cleans it up when the process exits.
func Start(ctx context.Context, cfg StartConfig) (*Process, error) {
	// Auto-generate MCP config if needed
	var tempMCPFile string
	if cfg.MCPConfig == "" && cfg.MCPServer != nil && cfg.MCPServer.IntercomdBinary != "" {
		path, err := WriteMCPConfig(*cfg.MCPServer)
		if err != nil {
			return nil, fmt.Errorf("generate mcp config: %w", err)
		}
		cfg.MCPConfig = path
		tempMCPFile = path
	}

	args, err := buildArgs(cfg)
	if err != nil {
		if tempMCPFile != "" {
			os.Remove(tempMCPFile)
		}
		return nil, err
	}

	cmd := exec.CommandContext(ctx, args[0], args[1:]...)
	cmd.Dir = cfg.WorkDir
	if len(cfg.Env) > 0 {
		cmd.Env = append(cmd.Environ(), cfg.Env...)
	}

	stdin, err := cmd.StdinPipe()
	if err != nil {
		if tempMCPFile != "" {
			os.Remove(tempMCPFile)
		}
		return nil, fmt.Errorf("stdin pipe: %w", err)
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		if tempMCPFile != "" {
			os.Remove(tempMCPFile)
		}
		return nil, fmt.Errorf("stdout pipe: %w", err)
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		if tempMCPFile != "" {
			os.Remove(tempMCPFile)
		}
		return nil, fmt.Errorf("stderr pipe: %w", err)
	}

	if err := cmd.Start(); err != nil {
		if tempMCPFile != "" {
			os.Remove(tempMCPFile)
		}
		return nil, fmt.Errorf("start %s: %w", args[0], err)
	}

	p := &Process{
		cmd:     cmd,
		stdin:   stdin,
		stdout:  stdout,
		stderr:  stderr,
		done:    make(chan struct{}),
		started: time.Now(),
	}

	// Wait in background, clean up temp MCP config on exit
	go func() {
		p.err = cmd.Wait()
		if tempMCPFile != "" {
			os.Remove(tempMCPFile)
		}
		close(p.done)
	}()

	return p, nil
}

// ReadFrames reads JSON-lines from stdout, calling handler for each frame.
// Blocks until stdout is closed or ctx is cancelled.
func (p *Process) ReadFrames(ctx context.Context, handler func(Frame)) error {
	scanner := bufio.NewScanner(p.stdout)
	scanner.Buffer(make([]byte, 0, 256*1024), 1024*1024) // 1MB max line

	for scanner.Scan() {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		line := scanner.Bytes()
		if len(line) == 0 {
			continue
		}

		var frame Frame
		if err := json.Unmarshal(line, &frame); err != nil {
			slog.Debug("non-JSON stdout line", "line", string(line))
			// Treat non-JSON as text content
			handler(Frame{Type: "text", Content: string(line)})
			continue
		}
		handler(frame)
	}
	return scanner.Err()
}

// WriteStdin sends data to the subprocess stdin.
func (p *Process) WriteStdin(data string) error {
	_, err := io.WriteString(p.stdin, data+"\n")
	return err
}

// CloseStdin signals no more input.
func (p *Process) CloseStdin() error {
	return p.stdin.Close()
}

// Kill terminates the subprocess.
func (p *Process) Kill() error {
	if p.cmd.Process != nil {
		return p.cmd.Process.Kill()
	}
	return nil
}

// Wait blocks until the subprocess exits.
func (p *Process) Wait() error {
	<-p.done
	return p.err
}

// Done returns a channel that closes when the subprocess exits.
func (p *Process) Done() <-chan struct{} {
	return p.done
}

// Duration returns how long the process has been running.
func (p *Process) Duration() time.Duration {
	return time.Since(p.started)
}

// DrainStderr reads all stderr and returns it.
func (p *Process) DrainStderr() string {
	data, _ := io.ReadAll(p.stderr)
	return string(data)
}

func buildArgs(cfg StartConfig) ([]string, error) {
	switch cfg.Runtime {
	case "claude":
		args := []string{"claude", "-p", cfg.Prompt, "--output-format", "stream-json"}
		if cfg.Model != "" {
			args = append(args, "--model", cfg.Model)
		}
		if cfg.MCPConfig != "" {
			args = append(args, "--mcp-config", cfg.MCPConfig)
		}
		args = append(args, cfg.Args...)
		return args, nil

	case "codex":
		args := []string{"codex", "exec",
			"--dangerously-bypass-approvals-and-sandbox",
			"-q", cfg.Prompt}
		if cfg.Model != "" {
			args = append(args, "--model", cfg.Model)
		}
		args = append(args, cfg.Args...)
		return args, nil

	case "gemini":
		args := []string{"gemini", "--prompt", cfg.Prompt}
		if cfg.Model != "" {
			args = append(args, "--model", cfg.Model)
		}
		args = append(args, cfg.Args...)
		return args, nil

	default:
		return nil, fmt.Errorf("unknown runtime: %s", cfg.Runtime)
	}
}
