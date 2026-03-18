package container

import (
	"bufio"
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

const (
	containerRuntimeBin = "docker"

	// MaxOutputSize is the maximum output buffer size (1 MiB) before truncation.
	maxOutputSize = 1_048_576

	// Default container timeout (5 minutes).
	defaultTimeoutMs = 300_000

	// Default idle timeout (30 minutes).
	defaultIdleTimeoutMs = 1_800_000

	// Startup timeout — container must produce first output within this window.
	startupTimeout = 30 * time.Second

	// Maximum UDS frame size (4 MiB).
	maxUDSFrameSize = 4 * 1024 * 1024
)

// RunConfig configures a container execution.
type RunConfig struct {
	ProjectRoot     string
	GroupsDir       string
	DataDir         string
	Timezone        string
	IdleTimeoutMs   uint64
	Allowlist       *MountAllowlist
	SessionMaxBytes int64
	ResultTimeoutMs uint64
}

// DefaultRunConfig returns a RunConfig with sensible defaults.
func DefaultRunConfig() RunConfig {
	cwd, _ := os.Getwd()
	return RunConfig{
		ProjectRoot:     cwd,
		GroupsDir:       "groups",
		DataDir:         "data",
		Timezone:        "UTC",
		IdleTimeoutMs:   defaultIdleTimeoutMs,
		SessionMaxBytes: 512 * 1024,
		ResultTimeoutMs: 180_000,
	}
}

// RunResult holds the outcome of a container run.
type RunResult struct {
	Output        ContainerOutput
	ContainerName string
	Duration      time.Duration
}

// OutputCallback is invoked for each streaming output frame.
type OutputCallback func(ContainerOutput)

// OnSpawnCallback is invoked after the container process is spawned.
type OnSpawnCallback func(containerName string)

// RunContainerAgent spawns a Docker container, writes input to stdin,
// streams output via UDS or stdout markers, and manages lifecycle.
func RunContainerAgent(
	ctx context.Context,
	group *GroupInfo,
	input *ContainerInput,
	runtime RuntimeKind,
	isMain bool,
	config *RunConfig,
	onOutput OutputCallback,
	onSpawn OnSpawnCallback,
) (*RunResult, error) {
	start := time.Now()

	// Ensure group directory exists
	groupDir := filepath.Join(config.GroupsDir, group.Folder)
	os.MkdirAll(groupDir, 0o755)
	logsDir := filepath.Join(groupDir, "logs")
	os.MkdirAll(logsDir, 0o755)

	// Build mounts and container args
	mounts := BuildVolumeMounts(group, isMain, runtime,
		config.ProjectRoot, config.GroupsDir, config.DataDir, config.Allowlist)

	name := ContainerName(group.Folder)
	image := ContainerImage(runtime)
	containerArgs := BuildContainerArgs(mounts, name, image, config.Timezone)

	// Bind UDS listener for container output before spawning.
	ipcDir := filepath.Join(config.DataDir, "ipc", group.Folder)
	udsSocketPath := filepath.Join(ipcDir, "output.sock")
	os.Remove(udsSocketPath) // remove stale socket

	var udsListener net.Listener
	if l, err := net.Listen("unix", udsSocketPath); err == nil {
		slog.Debug("UDS output listener bound", "path", udsSocketPath)
		udsListener = l
	} else {
		slog.Warn("failed to bind UDS listener, using stdout-only",
			"path", udsSocketPath, "error", err)
	}
	defer func() {
		if udsListener != nil {
			udsListener.Close()
		}
		os.Remove(udsSocketPath)
	}()

	slog.Info("spawning container agent",
		"group", group.Name,
		"container_name", name,
		"mount_count", len(mounts),
		"is_main", isMain,
		"runtime", string(runtime),
		"uds_enabled", udsListener != nil,
	)

	// Spawn the container process
	cmd := exec.CommandContext(ctx, containerRuntimeBin, containerArgs...)
	cmd.Stderr = nil // we'll read it manually

	stdinPipe, err := cmd.StdinPipe()
	if err != nil {
		return nil, fmt.Errorf("stdin pipe: %w", err)
	}
	stdoutPipe, err := cmd.StdoutPipe()
	if err != nil {
		return nil, fmt.Errorf("stdout pipe: %w", err)
	}
	stderrPipe, err := cmd.StderrPipe()
	if err != nil {
		return nil, fmt.Errorf("stderr pipe: %w", err)
	}

	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("failed to spawn container: %w", err)
	}

	// Notify caller of container name
	if onSpawn != nil {
		onSpawn(name)
	}

	// Write input + secrets to stdin
	stdinInput := *input
	stdinInput.Secrets = ReadSecrets(config.ProjectRoot)
	inputJSON, err := json.Marshal(&stdinInput)
	if err != nil {
		cmd.Process.Kill()
		return nil, fmt.Errorf("marshal input: %w", err)
	}

	go func() {
		stdinPipe.Write(inputJSON)
		stdinPipe.Close()
	}()

	// Timeout configuration
	containerTimeout := uint64(defaultTimeoutMs)
	if group.ContainerConfig != nil && group.ContainerConfig.Timeout != nil {
		containerTimeout = *group.ContainerConfig.Timeout
	}
	timeoutMs := containerTimeout
	if config.IdleTimeoutMs+30_000 > timeoutMs {
		timeoutMs = config.IdleTimeoutMs + 30_000
	}

	// Shared state
	var (
		timedOut     atomic.Bool
		hadOutput    atomic.Bool
		hadResult    atomic.Bool
		lastActivity atomic.Value // time.Time
		newSessionID atomic.Value // string
	)
	lastActivity.Store(time.Now())

	// Timeout watchdog goroutine
	watchdogCtx, watchdogCancel := context.WithCancel(ctx)
	defer watchdogCancel()

	go func() {
		hasReceivedActivity := false
		spawnTime := time.Now()
		resultTimeoutDur := time.Duration(config.ResultTimeoutMs) * time.Millisecond

		ticker := time.NewTicker(500 * time.Millisecond)
		defer ticker.Stop()

		for {
			select {
			case <-watchdogCtx.Done():
				return
			case <-ticker.C:
			}

			last := lastActivity.Load().(time.Time)
			if last.After(spawnTime) {
				hasReceivedActivity = true
			}

			effectiveTimeout := time.Duration(timeoutMs) * time.Millisecond
			if !hasReceivedActivity {
				effectiveTimeout = startupTimeout
			}

			if time.Since(last) >= effectiveTimeout {
				timedOut.Store(true)
				if hasReceivedActivity {
					slog.Error("container timeout, stopping", "container_name", name)
				} else {
					slog.Error("container startup timeout — no output received, stopping",
						"container_name", name,
						"startup_timeout_secs", int(startupTimeout.Seconds()))
				}
				exec.Command(containerRuntimeBin, "stop", name).Run()
				return
			}

			// Result timeout check
			if hasReceivedActivity && resultTimeoutDur > 0 &&
				!hadResult.Load() && time.Since(spawnTime) >= resultTimeoutDur {
				timedOut.Store(true)
				slog.Error("result timeout — container active but no result frame, stopping",
					"container_name", name,
					"elapsed_secs", int(time.Since(spawnTime).Seconds()),
					"result_timeout_secs", int(resultTimeoutDur.Seconds()))
				exec.Command(containerRuntimeBin, "stop", name).Run()
				return
			}
		}
	}()

	// Stream output concurrently from stdout, stderr, and UDS
	var (
		stdoutTotal strings.Builder
		stderrTotal strings.Builder
		stdoutTrunc bool
		stderrTrunc bool
		wg          sync.WaitGroup
	)

	// UDS reader goroutine
	if udsListener != nil {
		wg.Add(1)
		go func() {
			defer wg.Done()
			conn, err := udsListener.Accept()
			if err != nil {
				return
			}
			defer conn.Close()

			slog.Info("UDS connection accepted", "group", group.Name)
			for {
				frame, err := readUDSFrame(conn)
				if err != nil {
					if err != io.EOF {
						slog.Warn("UDS read error", "group", group.Name, "error", err)
					}
					return
				}

				var parsed ContainerOutput
				if err := json.Unmarshal([]byte(frame), &parsed); err != nil {
					slog.Warn("failed to parse UDS output frame",
						"group", group.Name, "error", err)
					continue
				}

				if parsed.NewSessionID != "" {
					newSessionID.Store(parsed.NewSessionID)
				}
				if parsed.Result != nil {
					hadResult.Store(true)
					// Write _close sentinel for one-shot containers
					closeDir := filepath.Join(ipcDir, "input")
					os.MkdirAll(closeDir, 0o755)
					os.WriteFile(filepath.Join(closeDir, "_close"), []byte(""), 0o644)
				}
				hadOutput.Store(true)
				lastActivity.Store(time.Now())

				if onOutput != nil {
					onOutput(parsed)
				}
			}
		}()
	}

	// Stderr reader goroutine
	wg.Add(1)
	go func() {
		defer wg.Done()
		scanner := bufio.NewScanner(stderrPipe)
		scanner.Buffer(make([]byte, 0, 64*1024), 256*1024)
		for scanner.Scan() {
			line := scanner.Text()
			if strings.TrimSpace(line) != "" {
				slog.Debug("container stderr", "container", group.Folder, "line", line)
			}
			if !stderrTrunc {
				if stderrTotal.Len()+len(line)+1 > maxOutputSize {
					stderrTrunc = true
					slog.Warn("container stderr truncated", "group", group.Name)
				} else {
					stderrTotal.WriteString(line)
					stderrTotal.WriteByte('\n')
				}
			}
		}
	}()

	// Stdout reader (main goroutine)
	var stdoutBuf strings.Builder
	udsActive := udsListener != nil
	scanner := bufio.NewScanner(stdoutPipe)
	scanner.Buffer(make([]byte, 0, 256*1024), 1024*1024)

	for scanner.Scan() {
		line := scanner.Text()

		// Accumulate for logging
		if !stdoutTrunc {
			if stdoutTotal.Len()+len(line)+1 > maxOutputSize {
				stdoutTrunc = true
				slog.Warn("container stdout truncated", "group", group.Name)
			} else {
				stdoutTotal.WriteString(line)
				stdoutTotal.WriteByte('\n')
			}
		}

		// Parse OUTPUT markers only when UDS is NOT active (fallback mode)
		if !udsActive && onOutput != nil {
			stdoutBuf.WriteString(line)
			stdoutBuf.WriteByte('\n')

			results, consumed := ExtractOutputMarkers(stdoutBuf.String())
			if consumed > 0 {
				remaining := stdoutBuf.String()[consumed:]
				stdoutBuf.Reset()
				stdoutBuf.WriteString(remaining)
			}
			for _, jsonStr := range results {
				var parsed ContainerOutput
				if err := json.Unmarshal([]byte(jsonStr), &parsed); err != nil {
					slog.Warn("failed to parse streamed output chunk",
						"group", group.Name, "error", err)
					continue
				}
				if parsed.NewSessionID != "" {
					newSessionID.Store(parsed.NewSessionID)
				}
				if parsed.Result != nil {
					hadResult.Store(true)
				}
				hadOutput.Store(true)
				lastActivity.Store(time.Now())
				onOutput(parsed)
			}
		}
	}

	// Wait for stderr and UDS goroutines
	wg.Wait()
	watchdogCancel()

	// Wait for process exit
	cmdErr := cmd.Wait()
	duration := time.Since(start)

	wasTimedOut := timedOut.Load()
	hadOut := hadOutput.Load()
	hadRes := hadResult.Load()
	var sessID string
	if v := newSessionID.Load(); v != nil {
		sessID = v.(string)
	}

	exitCode := -1
	if cmd.ProcessState != nil {
		exitCode = cmd.ProcessState.ExitCode()
	}

	// Write container log
	writeContainerLog(logsDir, group.Name, name, duration, exitCode, wasTimedOut,
		hadOut, mounts, stdoutTotal.String(), stdoutTrunc, stderrTotal.String(), stderrTrunc)

	// Handle timeout cases
	if wasTimedOut {
		if hadOut && hadRes {
			// Normal idle timeout after producing a result — not an error
			slog.Info("container timed out after output (idle cleanup)",
				"group", group.Name, "container_name", name,
				"duration_ms", duration.Milliseconds())
			return &RunResult{
				Output: ContainerOutput{
					Status:       StatusSuccess,
					NewSessionID: sessID,
				},
				ContainerName: name,
				Duration:      duration,
			}, nil
		}

		if hadOut && !hadRes {
			slog.Warn("container timed out with activity but no result — session may be bloated",
				"group", group.Name, "container_name", name,
				"duration_ms", duration.Milliseconds())
			return &RunResult{
				Output: ContainerOutput{
					Status: StatusError,
					Error:  "Result timeout — session auto-reset",
				},
				ContainerName: name,
				Duration:      duration,
			}, nil
		}

		slog.Error("container timed out with no output",
			"group", group.Name, "container_name", name,
			"duration_ms", duration.Milliseconds())
		return &RunResult{
			Output: ContainerOutput{
				Status: StatusError,
				Error:  fmt.Sprintf("Container timed out after %dms", containerTimeout),
			},
			ContainerName: name,
			Duration:      duration,
		}, nil
	}

	// Handle error exit
	if cmdErr != nil || exitCode != 0 {
		slog.Error("container exited with error",
			"group", group.Name, "exit_code", exitCode,
			"duration_ms", duration.Milliseconds())
		stderr := stderrTotal.String()
		tail := stderr
		if len(tail) > 2000 {
			tail = tail[len(tail)-2000:]
		}
		return &RunResult{
			Output: ContainerOutput{
				Status: StatusError,
				Error:  fmt.Sprintf("Container exited with code %d: %s", exitCode, tail),
			},
			ContainerName: name,
			Duration:      duration,
		}, nil
	}

	// Streaming mode: output was already dispatched via callbacks
	if onOutput != nil {
		slog.Info("container completed (streaming mode)",
			"group", group.Name, "duration_ms", duration.Milliseconds())
		return &RunResult{
			Output: ContainerOutput{
				Status:       StatusSuccess,
				NewSessionID: sessID,
			},
			ContainerName: name,
			Duration:      duration,
		}, nil
	}

	// Legacy mode: parse the last output marker pair from accumulated stdout
	results, _ := ExtractOutputMarkers(stdoutTotal.String())
	if len(results) > 0 {
		var output ContainerOutput
		if err := json.Unmarshal([]byte(results[len(results)-1]), &output); err != nil {
			slog.Error("failed to parse container output",
				"group", group.Name, "error", err)
			return &RunResult{
				Output: ContainerOutput{
					Status: StatusError,
					Error:  fmt.Sprintf("Failed to parse container output: %v", err),
				},
				ContainerName: name,
				Duration:      duration,
			}, nil
		}
		slog.Info("container completed",
			"group", group.Name, "duration_ms", duration.Milliseconds(),
			"status", output.Status)
		return &RunResult{
			Output:        output,
			ContainerName: name,
			Duration:      duration,
		}, nil
	}

	// Fallback: try parsing last non-empty line
	lines := strings.Split(strings.TrimSpace(stdoutTotal.String()), "\n")
	if len(lines) > 0 {
		lastLine := lines[len(lines)-1]
		var output ContainerOutput
		if err := json.Unmarshal([]byte(lastLine), &output); err == nil {
			return &RunResult{
				Output:        output,
				ContainerName: name,
				Duration:      duration,
			}, nil
		}
	}

	return &RunResult{
		Output: ContainerOutput{
			Status: StatusError,
			Error:  "No OUTPUT markers found and failed to parse last line",
		},
		ContainerName: name,
		Duration:      duration,
	}, nil
}

// readUDSFrame reads a length-prefixed frame from a Unix domain socket.
// Frame format: 4-byte big-endian length + UTF-8 JSON payload.
func readUDSFrame(conn net.Conn) (string, error) {
	lenBuf := make([]byte, 4)
	if _, err := io.ReadFull(conn, lenBuf); err != nil {
		return "", err
	}

	frameLen := int(binary.BigEndian.Uint32(lenBuf))
	if frameLen > maxUDSFrameSize {
		return "", fmt.Errorf("UDS frame too large: %d bytes (max %d)", frameLen, maxUDSFrameSize)
	}

	payload := make([]byte, frameLen)
	if _, err := io.ReadFull(conn, payload); err != nil {
		return "", err
	}

	return string(payload), nil
}

// EnsureRuntimeAvailable checks if the container runtime (docker) is available.
func EnsureRuntimeAvailable() error {
	out, err := exec.Command(containerRuntimeBin, "info").Output()
	if err != nil {
		return fmt.Errorf("container runtime not found: %w", err)
	}
	_ = out
	slog.Debug("container runtime available")
	return nil
}

// CleanupOrphans kills orphaned intercom containers from previous runs.
func CleanupOrphans() {
	out, err := exec.Command(containerRuntimeBin,
		"ps", "--filter", "name=intercom-", "--format", "{{.Names}}").Output()
	if err != nil {
		slog.Warn("failed to list orphaned containers", "error", err)
		return
	}

	names := strings.Split(strings.TrimSpace(string(out)), "\n")
	var stopped int
	for _, name := range names {
		if name == "" {
			continue
		}
		exec.Command(containerRuntimeBin, "stop", name).Run()
		stopped++
	}

	if stopped > 0 {
		slog.Info("stopped orphaned containers", "count", stopped)
	}
}

// StopContainer gracefully stops a container by name.
func StopContainer(containerName string) bool {
	out, err := exec.Command(containerRuntimeBin, "stop", containerName).CombinedOutput()
	if err != nil {
		slog.Warn("failed to stop container",
			"container_name", containerName, "error", string(out))
		return false
	}
	slog.Info("container stopped", "container_name", containerName)
	return true
}

// writeContainerLog writes a run log to the logs directory.
func writeContainerLog(
	logsDir, groupName, containerName string,
	duration time.Duration, exitCode int, timedOut, hadOutput bool,
	mounts []VolumeMount, stdout string, stdoutTrunc bool,
	stderr string, stderrTrunc bool,
) {
	timestamp := fmt.Sprintf("%d-%03d", time.Now().Unix(), time.Now().UnixMilli()%1000)
	logFile := filepath.Join(logsDir, fmt.Sprintf("container-%s.log", timestamp))
	isError := exitCode != 0 || timedOut

	var b strings.Builder
	timeoutLabel := ""
	if timedOut {
		timeoutLabel = " (TIMEOUT)"
	}
	fmt.Fprintf(&b, "=== Container Run Log%s ===\n", timeoutLabel)
	fmt.Fprintf(&b, "Timestamp: %s\n", timestamp)
	fmt.Fprintf(&b, "Group: %s\n", groupName)
	fmt.Fprintf(&b, "Container: %s\n", containerName)
	fmt.Fprintf(&b, "Duration: %dms\n", duration.Milliseconds())
	fmt.Fprintf(&b, "Exit Code: %d\n", exitCode)
	fmt.Fprintf(&b, "Had Streaming Output: %v\n\n", hadOutput)

	if isError {
		b.WriteString("=== Mounts ===\n")
		for _, m := range mounts {
			ro := ""
			if m.ReadOnly {
				ro = " (ro)"
			}
			fmt.Fprintf(&b, "%s -> %s%s\n", m.HostPath, m.ContainerPath, ro)
		}
		truncLabel := ""
		if stderrTrunc {
			truncLabel = " (TRUNCATED)"
		}
		fmt.Fprintf(&b, "\n=== Stderr%s ===\n%s\n", truncLabel, stderr)
		truncLabel = ""
		if stdoutTrunc {
			truncLabel = " (TRUNCATED)"
		}
		fmt.Fprintf(&b, "\n=== Stdout%s ===\n%s\n", truncLabel, stdout)
	} else {
		b.WriteString("=== Mounts ===\n")
		for _, m := range mounts {
			ro := ""
			if m.ReadOnly {
				ro = " (ro)"
			}
			fmt.Fprintf(&b, "%s%s\n", m.ContainerPath, ro)
		}
	}

	if err := os.WriteFile(logFile, []byte(b.String()), 0o644); err != nil {
		slog.Warn("failed to write container log", "log_file", logFile, "error", err)
	} else {
		slog.Debug("container log written", "log_file", logFile)
	}
}
