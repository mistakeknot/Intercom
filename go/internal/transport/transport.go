// Package transport defines Intercom's canonical message-passing surface.
//
// Each transport (telegram, signal, a2a) translates wire-specific inbound
// events into canonical A2A-shaped [InboundMessage] values and renders outbound
// [OutboundMessage] values back into wire-specific form. The routing layer,
// scheduler, and subprocess manager see only the canonical shape; they never
// reach into a transport's wire types.
//
// The canonical form is the A2A message envelope per docs/canon/intercom-transport-target.md.
// Telegram and Signal are A2A-Message adapters; A2A is the native form.
package transport

import "context"

// Transport is the wire abstraction implemented once per delivery channel.
// Implementations live in subpackages: transport/telegram, transport/signal,
// transport/a2a.
//
// A transport is long-lived: Subscribe is called once and yields inbound
// messages until ctx is cancelled. Send is called per outbound message.
// Implementations MUST be safe for concurrent Send calls; Subscribe is
// expected to be called once per process lifecycle.
type Transport interface {
	// Name returns the stable identifier ("telegram", "signal", "a2a").
	// Used for routing decisions and Health reports; never user-visible.
	Name() string

	// Send delivers a canonical message on this transport's wire.
	// The wire-specific recipient address is in msg.RecipientURI.
	// Returns a non-nil error if delivery cannot be attempted; transient
	// network failures should be retried internally by the implementation
	// up to its configured budget before surfacing.
	Send(ctx context.Context, msg OutboundMessage) error

	// Subscribe yields inbound messages translated from this transport's
	// wire into canonical form. The returned channel is closed when ctx
	// is cancelled or when the transport's connection is permanently lost.
	// Implementations MUST NOT buffer beyond ~100 messages; backpressure
	// is delivered to the wire (e.g., Telegram getUpdates pause).
	Subscribe(ctx context.Context) (<-chan InboundMessage, error)

	// Health reports the transport's operational state. Cheap; called
	// frequently by the /health endpoint and SessionStart hooks.
	Health(ctx context.Context) Health
}

// InboundMessage is the canonical form for a message arriving from any
// transport. Translates 1:1 to A2A SendMessageRequest with role=ROLE_USER.
type InboundMessage struct {
	// TransportName is the origin transport identifier matching [Transport.Name].
	TransportName string

	// SenderURI is the canonical sender identity:
	//   - telegram:<user_id>
	//   - signal:<phone_e164>
	//   - sylveste://agent/<name>  (for inbound A2A)
	SenderURI string

	// ContextID maps to the bead ID (durable conversation thread) per
	// docs/canon/intercom-transport-target.md §Sylveste-sprint↔A2A-Task adapter.
	// Empty for first-touch inbound messages where no bead exists yet;
	// the routing layer assigns a bead.
	ContextID string

	// Parts carries the message body per A2A: text, file, data. Implementations
	// translate wire-specific media (Telegram document/photo/sticker) into the
	// appropriate Part kind.
	Parts []Part

	// ReceivedAt is the wire-receipt time in unix nanoseconds. Distinct from
	// the originator's send time (which lives in WireMetadata if available).
	ReceivedAt int64

	// WireMetadata carries transport-specific fields needed for round-tripping
	// (e.g., telegram chat_id + message_id for reply-to). Routing/scheduler
	// MUST NOT read these directly; they pass through opaquely.
	WireMetadata map[string]string
}

// OutboundMessage is the canonical form for a message going out to any transport.
// Translates 1:1 from A2A Message with role=ROLE_AGENT.
type OutboundMessage struct {
	// RecipientURI is the canonical recipient identity (same scheme as SenderURI).
	RecipientURI string

	// ContextID is the bead ID this message belongs to. Required.
	ContextID string

	// TaskID is the A2A runtime task handle for this agent invocation.
	// Distinct from ContextID: TaskID is per-run, ContextID is per-conversation.
	TaskID string

	// Parts carries the message body.
	Parts []Part

	// WireMetadata carries renderer hints (e.g., telegram reply_to_msg_id,
	// signal quote_id). Implementations consume what they recognize; unknown
	// keys are ignored without error.
	WireMetadata map[string]string
}

// Part is one segment of a canonical message body, matching A2A part kinds.
// Most messages have a single PartText. File/data parts are used for
// attachments and structured payloads.
type Part struct {
	Kind     PartKind
	Text     string // populated when Kind == PartText
	FileURI  string // populated when Kind == PartFile (canonical URI, not wire-specific)
	FileName string // populated when Kind == PartFile (display name)
	MIMEType string // populated when Kind == PartFile or Kind == PartData
	Data     []byte // populated when Kind == PartData
}

// PartKind enumerates the part kinds defined by A2A.
type PartKind int

const (
	// PartText is the common case: UTF-8 text body.
	PartText PartKind = iota
	// PartFile is an attachment referenced by URI plus optional display name.
	PartFile
	// PartData is structured payload bytes with a MIMEType (e.g., application/json).
	PartData
)

// String returns the canonical name of the part kind.
func (k PartKind) String() string {
	switch k {
	case PartText:
		return "text"
	case PartFile:
		return "file"
	case PartData:
		return "data"
	default:
		return "unknown"
	}
}

// Health reports a transport's operational status. Returned by [Transport.Health].
type Health struct {
	// Healthy is true when the transport is connected and able to send/receive.
	Healthy bool

	// LastInboundAt is the unix-nanos timestamp of the most recent inbound
	// message, or 0 if none received this process lifetime.
	LastInboundAt int64

	// LastOutboundAt is the unix-nanos timestamp of the most recent successful
	// outbound send, or 0 if none sent this process lifetime.
	LastOutboundAt int64

	// Reason is a short human-readable status when Healthy is false.
	// Empty when Healthy is true.
	Reason string
}
