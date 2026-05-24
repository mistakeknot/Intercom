// Package a2a implements Intercom's native Agent2Agent (A2A) transport.
//
// A2A is the canonical wire shape; sibling transports (telegram, signal) are
// adapters that translate to/from these types. See
// docs/canon/intercom-transport-target.md for the protocol-level decision.
//
// The wire format is JSON over HTTP. The endpoint set follows A2A spec §11.3:
// POST /messages, POST /messages:stream, GET /tasks/{id}, GET /tasks/{id}:subscribe,
// GET /tasks, POST /tasks/{id}:cancel, plus GET /.well-known/agent.json for discovery.
//
// This package currently implements the synchronous inbound path (POST /messages)
// and Agent Card discovery (GET /.well-known/agent.json). Streaming, task store,
// outbound client, and OAuth2 authentication land in sub-beads under sylveste-ewy3.4.1.
package a2a

import "encoding/json"

// Role enumerates the A2A message roles.
type Role string

const (
	RoleUser        Role = "ROLE_USER"
	RoleAgent       Role = "ROLE_AGENT"
	RoleUnspecified Role = "ROLE_UNSPECIFIED"
)

// Message is an A2A message envelope (spec §4.1).
//
// MessageID is required and unique within a context. ContextID is the durable
// conversation identifier — in Sylveste this maps to the bead ID. TaskID is the
// per-run runtime handle; populated when the message belongs to a Task.
type Message struct {
	MessageID  string            `json:"messageId"`
	Role       Role              `json:"role"`
	Parts      []Part            `json:"parts"`
	ContextID  string            `json:"contextId,omitempty"`
	TaskID     string            `json:"taskId,omitempty"`
	Metadata   map[string]string `json:"metadata,omitempty"`
	Extensions []string          `json:"extensions,omitempty"`
}

// Part is one segment of a message body. Exactly one of Text/File/Data is set.
// On the wire the part type is conveyed by which field is present.
type Part struct {
	Text string    `json:"text,omitempty"`
	File *FilePart `json:"file,omitempty"`
	Data *DataPart `json:"data,omitempty"`
}

// FilePart is an attachment reference.
type FilePart struct {
	URI      string `json:"uri"`
	Name     string `json:"name,omitempty"`
	MIMEType string `json:"mimeType,omitempty"`
}

// DataPart is a structured payload with a media type.
type DataPart struct {
	MIMEType string          `json:"mimeType"`
	Bytes    json.RawMessage `json:"bytes"`
}

// SendMessageRequest is the body of POST /messages (spec §4.1.SendMessage).
type SendMessageRequest struct {
	Message       Message                  `json:"message"`
	Configuration *SendMessageConfiguration `json:"configuration,omitempty"`
}

// SendMessageConfiguration controls per-request behavior (spec §4.1).
// Acceptable accept modes inform whether the server may return a synchronous
// Message reply or must return a Task handle for asynchronous follow-up.
type SendMessageConfiguration struct {
	AcceptedOutputModes []string `json:"acceptedOutputModes,omitempty"`
	HistoryLength       int      `json:"historyLength,omitempty"`
	Blocking            bool     `json:"blocking,omitempty"`
}

// SendMessageResponse is the result of POST /messages. Exactly one of
// Message (synchronous reply) or Task (async handle) is populated.
type SendMessageResponse struct {
	Message *Message `json:"message,omitempty"`
	Task    *Task    `json:"task,omitempty"`
}

// ListTasksResponse is the body of GET /tasks. NextCursor is empty when no
// further pages exist; callers pass it back as the `cursor` query param to
// fetch the next page.
type ListTasksResponse struct {
	Tasks      []Task `json:"tasks"`
	NextCursor string `json:"nextCursor,omitempty"`
}

// StreamEventKind discriminates the three SSE event variants the streaming
// endpoints emit. The wire `event:` field carries the canonical kind name.
type StreamEventKind string

const (
	// StreamEventStatus is a TaskStatusUpdateEvent — most events on the wire.
	StreamEventStatus StreamEventKind = "TaskStatusUpdateEvent"
	// StreamEventArtifact is a TaskArtifactUpdateEvent — emitted as the agent
	// produces output artifacts (files, structured payloads).
	StreamEventArtifact StreamEventKind = "TaskArtifactUpdateEvent"
	// StreamEventMessage is a MessageEvent — emitted when the agent sends a
	// synchronous reply Message inline during streaming.
	StreamEventMessage StreamEventKind = "MessageEvent"
)

// StreamEvent is the union shape passed between the broker and SSE handlers.
// Exactly one of Status / Artifact / Message is set, matching Kind. The
// handler renders it onto the wire as `event: <kind>\ndata: <json>\n\n` with
// the appropriate sub-struct JSON-encoded as data.
type StreamEvent struct {
	Kind     StreamEventKind         `json:"-"`
	Status   *TaskStatusUpdateEvent  `json:"status,omitempty"`
	Artifact *TaskArtifactUpdateEvent `json:"artifact,omitempty"`
	Message  *Message                `json:"message,omitempty"`
}

// TaskStatusUpdateEvent is the SSE payload for a task lifecycle transition
// (spec §4.2). Final=true on terminal transitions (COMPLETED, CANCELLED,
// FAILED, REJECTED); clients close their connection on Final.
type TaskStatusUpdateEvent struct {
	TaskID    string     `json:"taskId"`
	ContextID string     `json:"contextId,omitempty"`
	Status    TaskStatus `json:"status"`
	Final     bool       `json:"final"`
}

// TaskArtifactUpdateEvent is the SSE payload for artifact streaming. LastChunk
// signals the artifact is complete; Append=true means parts should be merged
// into an existing artifact rather than replacing.
type TaskArtifactUpdateEvent struct {
	TaskID    string   `json:"taskId"`
	ContextID string   `json:"contextId,omitempty"`
	Artifact  Artifact `json:"artifact"`
	Append    bool     `json:"append,omitempty"`
	LastChunk bool     `json:"lastChunk,omitempty"`
}

// Task is a long-running agent invocation handle (spec §4.2).
type Task struct {
	ID        string            `json:"id"`
	ContextID string            `json:"contextId"`
	Status    TaskStatus        `json:"status"`
	Artifacts []Artifact        `json:"artifacts,omitempty"`
	History   []Message         `json:"history,omitempty"`
	Metadata  map[string]string `json:"metadata,omitempty"`
}

// TaskState enumerates the lifecycle states a Task progresses through.
// See docs/canon/intercom-transport-target.md §Mapping rules #5 for the
// Sylveste-side correspondence.
type TaskState string

const (
	TaskStateUnspecified   TaskState = "TASK_STATE_UNSPECIFIED"
	TaskStateSubmitted     TaskState = "TASK_STATE_SUBMITTED"
	TaskStateWorking       TaskState = "TASK_STATE_WORKING"
	TaskStateInputRequired TaskState = "TASK_STATE_INPUT_REQUIRED"
	TaskStateAuthRequired  TaskState = "TASK_STATE_AUTH_REQUIRED"
	TaskStateCompleted     TaskState = "TASK_STATE_COMPLETED"
	TaskStateCancelled     TaskState = "TASK_STATE_CANCELLED"
	TaskStateFailed        TaskState = "TASK_STATE_FAILED"
	TaskStateRejected      TaskState = "TASK_STATE_REJECTED"
)

// TaskStatus carries the current state plus its observed timestamp.
type TaskStatus struct {
	State     TaskState `json:"state"`
	Message   *Message  `json:"message,omitempty"`
	Timestamp string    `json:"timestamp,omitempty"`
}

// Artifact references a file or structured payload produced by a Task.
type Artifact struct {
	ArtifactID  string `json:"artifactId"`
	Name        string `json:"name,omitempty"`
	Description string `json:"description,omitempty"`
	Parts       []Part `json:"parts"`
}

// AgentCard advertises an agent's identity, capabilities, and skills (spec §8).
// Returned by GET /.well-known/agent.json.
type AgentCard struct {
	ProtocolVersion                   string                    `json:"protocolVersion"`
	Name                              string                    `json:"name"`
	Description                       string                    `json:"description"`
	URL                               string                    `json:"url"`
	Version                           string                    `json:"version"`
	Capabilities                      AgentCapabilities         `json:"capabilities"`
	DefaultInputModes                 []string                  `json:"defaultInputModes,omitempty"`
	DefaultOutputModes                []string                  `json:"defaultOutputModes,omitempty"`
	Skills                            []AgentSkill              `json:"skills,omitempty"`
	SecuritySchemes                   map[string]SecurityScheme `json:"securitySchemes,omitempty"`
	Security                          []map[string][]string     `json:"security,omitempty"`
	SupportsAuthenticatedExtendedCard bool                      `json:"supportsAuthenticatedExtendedCard,omitempty"`
}

// AgentCapabilities declares optional A2A features the agent supports.
type AgentCapabilities struct {
	Streaming         bool `json:"streaming"`
	PushNotifications bool `json:"pushNotifications"`
	ExtendedCard      bool `json:"extendedCard,omitempty"`
}

// AgentSkill describes one capability surface the agent exposes (spec §8.4).
type AgentSkill struct {
	ID          string   `json:"id"`
	Name        string   `json:"name"`
	Description string   `json:"description,omitempty"`
	Tags        []string `json:"tags,omitempty"`
	Examples    []string `json:"examples,omitempty"`
	InputModes  []string `json:"inputModes,omitempty"`
	OutputModes []string `json:"outputModes,omitempty"`
}

// SecurityScheme declares a supported authentication mechanism (spec §7).
// Sylveste advertises OAuth2 with Resource Indicators per Gridfire-v1.
type SecurityScheme struct {
	Type             string                 `json:"type"`
	Description      string                 `json:"description,omitempty"`
	Flows            map[string]OAuth2Flow  `json:"flows,omitempty"`
	ResourceIndicators []string             `json:"resourceIndicators,omitempty"`
}

// OAuth2Flow describes a single OAuth2 grant (spec §7.2).
type OAuth2Flow struct {
	AuthorizationURL string            `json:"authorizationUrl,omitempty"`
	TokenURL         string            `json:"tokenUrl,omitempty"`
	RefreshURL       string            `json:"refreshUrl,omitempty"`
	Scopes           map[string]string `json:"scopes,omitempty"`
}
