package a2a

import (
	"time"

	"github.com/mistakeknot/intercom/internal/transport"
)

// SenderURIForAgent returns the canonical Sylveste agent identity URI.
// Per docs/canon/intercom-transport-target.md §Identity, the scheme is
// sylveste://agent/<name>.
func SenderURIForAgent(name string) string {
	return "sylveste://agent/" + name
}

// ToInbound translates an A2A Message into the canonical transport.InboundMessage.
//
// The agentName parameter identifies the agent receiving this message; it appears
// in WireMetadata so the routing layer can attribute and reply on the right
// endpoint when multiple Sylveste agents are co-hosted in one Intercom binary.
//
// Per the canon doc, A2A Role MUST be ROLE_USER for inbound; ROLE_AGENT messages
// arriving on this surface represent agent-to-agent invocations and are still
// translated, with SenderURI carrying the calling agent's identity (taken from
// the message metadata if present, otherwise empty for the routing layer to fill).
func ToInbound(msg Message, agentName string) transport.InboundMessage {
	in := transport.InboundMessage{
		TransportName: "a2a",
		SenderURI:     extractSenderURI(msg),
		ContextID:     msg.ContextID,
		Parts:         translatePartsToCanonical(msg.Parts),
		ReceivedAt:    time.Now().UnixNano(),
		WireMetadata:  map[string]string{},
	}
	in.WireMetadata["a2a.messageId"] = msg.MessageID
	in.WireMetadata["a2a.role"] = string(msg.Role)
	if agentName != "" {
		in.WireMetadata["a2a.recipientAgent"] = agentName
	}
	if msg.TaskID != "" {
		in.WireMetadata["a2a.taskId"] = msg.TaskID
	}
	for k, v := range msg.Metadata {
		in.WireMetadata["a2a.meta."+k] = v
	}
	return in
}

// FromOutbound translates a canonical transport.OutboundMessage into an A2A Message
// with role=ROLE_AGENT. Used by the outbound client and by streaming responses
// emitted back over the same HTTP connection.
func FromOutbound(out transport.OutboundMessage) Message {
	msg := Message{
		MessageID: generateMessageID(),
		Role:      RoleAgent,
		Parts:     translatePartsFromCanonical(out.Parts),
		ContextID: out.ContextID,
		TaskID:    out.TaskID,
		Metadata:  map[string]string{},
	}
	for k, v := range out.WireMetadata {
		// Recognize a2a.meta.* hints; pass the rest through unchanged so peer
		// implementations can read non-A2A WireMetadata they emitted previously.
		msg.Metadata[k] = v
	}
	return msg
}

func translatePartsToCanonical(parts []Part) []transport.Part {
	if len(parts) == 0 {
		return nil
	}
	out := make([]transport.Part, 0, len(parts))
	for _, p := range parts {
		switch {
		case p.Text != "":
			out = append(out, transport.Part{Kind: transport.PartText, Text: p.Text})
		case p.File != nil:
			out = append(out, transport.Part{
				Kind:     transport.PartFile,
				FileURI:  p.File.URI,
				FileName: p.File.Name,
				MIMEType: p.File.MIMEType,
			})
		case p.Data != nil:
			out = append(out, transport.Part{
				Kind:     transport.PartData,
				MIMEType: p.Data.MIMEType,
				Data:     []byte(p.Data.Bytes),
			})
		}
	}
	return out
}

func translatePartsFromCanonical(parts []transport.Part) []Part {
	if len(parts) == 0 {
		return nil
	}
	out := make([]Part, 0, len(parts))
	for _, p := range parts {
		switch p.Kind {
		case transport.PartText:
			out = append(out, Part{Text: p.Text})
		case transport.PartFile:
			out = append(out, Part{File: &FilePart{
				URI:      p.FileURI,
				Name:     p.FileName,
				MIMEType: p.MIMEType,
			}})
		case transport.PartData:
			out = append(out, Part{Data: &DataPart{
				MIMEType: p.MIMEType,
				Bytes:    p.Data,
			}})
		}
	}
	return out
}

// extractSenderURI pulls the canonical sender identity from an A2A Message.
//
// Native A2A peers populate Metadata["sylveste.senderUri"] with the calling
// agent's sylveste://agent/<name> URI. External A2A peers without that hint
// produce an empty SenderURI; the routing layer then anchors them via the
// authenticated identity from the OAuth2 token (sub-bead: OAuth integration).
func extractSenderURI(msg Message) string {
	if uri, ok := msg.Metadata["sylveste.senderUri"]; ok {
		return uri
	}
	return ""
}

// generateMessageID returns a unique A2A message ID. The format is
// a2a-<unix-nanos>-<counter>; counter is monotonic within process to avoid
// collisions within the same nanosecond.
var msgCounter uint64

func generateMessageID() string {
	now := time.Now().UnixNano()
	msgCounter++
	return formatMessageID(now, msgCounter)
}
