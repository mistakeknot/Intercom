// Package container implements Docker container execution for agent runtimes.
//
// Port of the Rust container module (intercomd/src/container/) to Go.
// Manages the full container lifecycle: build volume mounts, inject secrets,
// spawn Docker containers, stream output via UDS or stdout markers, and
// handle timeouts and cleanup.
package container

// RuntimeKind identifies which agent runtime to use.
type RuntimeKind string

const (
	RuntimeClaude RuntimeKind = "claude"
	RuntimeGemini RuntimeKind = "gemini"
	RuntimeCodex  RuntimeKind = "codex"
)

// ContainerInput is the JSON payload written to container stdin.
// Field names use camelCase to match the Node wire format.
type ContainerInput struct {
	Prompt          string            `json:"prompt"`
	SessionID       string            `json:"sessionId,omitempty"`
	GroupFolder     string            `json:"groupFolder"`
	ChatJID         string            `json:"chatJid"`
	IsMain          bool              `json:"isMain"`
	IsScheduledTask *bool             `json:"isScheduledTask,omitempty"`
	AssistantName   string            `json:"assistantName,omitempty"`
	Model           string            `json:"model,omitempty"`
	Secrets         map[string]string `json:"secrets,omitempty"`
	PreviousContext string            `json:"previousContext,omitempty"`
}

// ContainerOutput is extracted from container stdout between OUTPUT markers
// or received via UDS frames.
type ContainerOutput struct {
	Status       ContainerStatus `json:"status"`
	Result       *string         `json:"result"`
	NewSessionID string          `json:"newSessionId,omitempty"`
	Error        string          `json:"error,omitempty"`
	Model        string          `json:"model,omitempty"`
	Event        *StreamEvent    `json:"event,omitempty"`
}

// ContainerStatus represents the outcome of a container run.
type ContainerStatus string

const (
	StatusSuccess ContainerStatus = "success"
	StatusError   ContainerStatus = "error"
)

// StreamEvent is an incremental streaming event from the container.
// Tag values use snake_case to match the Node wire format.
type StreamEvent struct {
	Type      string `json:"type"`                // "tool_start" or "text_delta"
	ToolName  string `json:"toolName,omitempty"`  // for tool_start
	ToolInput string `json:"toolInput,omitempty"` // for tool_start
	Text      string `json:"text,omitempty"`      // for text_delta
}

// VolumeMount specifies a bind mount for container execution.
type VolumeMount struct {
	HostPath      string
	ContainerPath string
	ReadOnly      bool
	Exclude       []string // subdirectory names to hide via tmpfs overlay
}

// Sentinel markers for robust output parsing.
// Must match the constants in container agent-runner code and intercom-core.
const (
	OutputStartMarker = "---INTERCOM_OUTPUT_START---"
	OutputEndMarker   = "---INTERCOM_OUTPUT_END---"
)

// ContainerImage returns the Docker image name for a runtime.
func ContainerImage(runtime RuntimeKind) string {
	switch runtime {
	case RuntimeGemini:
		return "intercom-agent-gemini:latest"
	case RuntimeCodex:
		return "intercom-agent-codex:latest"
	default:
		return "intercom-agent:latest"
	}
}

// RunnerDirName returns the runner source directory name for a runtime.
func RunnerDirName(runtime RuntimeKind) string {
	switch runtime {
	case RuntimeGemini:
		return "gemini-runner"
	case RuntimeCodex:
		return "codex-runner"
	default:
		return "agent-runner"
	}
}

// RunnerContainerPath returns the container mount path for runner source.
func RunnerContainerPath(runtime RuntimeKind) string {
	return "/app/" + RunnerDirName(runtime) + "/src"
}

// ExtractOutputMarkers parses OUTPUT marker pairs from a buffer.
// Returns extracted JSON strings and the number of bytes consumed.
func ExtractOutputMarkers(buf string) (results []string, consumed int) {
	searchFrom := 0
	for {
		startIdx := indexOf(buf[searchFrom:], OutputStartMarker)
		if startIdx < 0 {
			break
		}
		startIdx += searchFrom

		afterStart := startIdx + len(OutputStartMarker)
		endIdx := indexOf(buf[afterStart:], OutputEndMarker)
		if endIdx < 0 {
			break // incomplete pair
		}
		endIdx += afterStart

		jsonStr := trimSpace(buf[afterStart:endIdx])
		results = append(results, jsonStr)

		consumed = endIdx + len(OutputEndMarker)
		searchFrom = consumed
	}
	return
}

func indexOf(s, substr string) int {
	for i := 0; i+len(substr) <= len(s); i++ {
		if s[i:i+len(substr)] == substr {
			return i
		}
	}
	return -1
}

func trimSpace(s string) string {
	start, end := 0, len(s)
	for start < end && (s[start] == ' ' || s[start] == '\t' || s[start] == '\n' || s[start] == '\r') {
		start++
	}
	for end > start && (s[end-1] == ' ' || s[end-1] == '\t' || s[end-1] == '\n' || s[end-1] == '\r') {
		end--
	}
	return s[start:end]
}
