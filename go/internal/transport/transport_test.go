package transport

import (
	"context"
	"testing"
)

// TestPartKindString verifies the canonical string names.
func TestPartKindString(t *testing.T) {
	cases := []struct {
		kind PartKind
		want string
	}{
		{PartText, "text"},
		{PartFile, "file"},
		{PartData, "data"},
		{PartKind(99), "unknown"},
	}
	for _, c := range cases {
		if got := c.kind.String(); got != c.want {
			t.Errorf("PartKind(%d).String() = %q, want %q", c.kind, got, c.want)
		}
	}
}

// mockTransport is a minimal implementation used to verify the interface
// compiles and is implementable. Not exported; lives only for the test.
type mockTransport struct {
	name string
	out  chan InboundMessage
}

func (m *mockTransport) Name() string { return m.name }

func (m *mockTransport) Send(_ context.Context, _ OutboundMessage) error { return nil }

func (m *mockTransport) Subscribe(ctx context.Context) (<-chan InboundMessage, error) {
	go func() {
		<-ctx.Done()
		close(m.out)
	}()
	return m.out, nil
}

func (m *mockTransport) Health(_ context.Context) Health {
	return Health{Healthy: true}
}

// TestTransportInterface confirms the interface shape compiles against
// a concrete implementation. Compile-time check; the assertion happens at
// the var declaration.
func TestTransportInterface(t *testing.T) {
	var _ Transport = &mockTransport{name: "mock", out: make(chan InboundMessage)}
}

// TestInboundMessageZero confirms the zero value is usable (no nil-map panics).
func TestInboundMessageZero(t *testing.T) {
	var m InboundMessage
	if m.WireMetadata != nil {
		t.Errorf("zero InboundMessage should have nil WireMetadata, got %v", m.WireMetadata)
	}
	// Verifies callers must initialize WireMetadata if they intend to write to it.
	if len(m.Parts) != 0 {
		t.Errorf("zero InboundMessage should have empty Parts, got %v", m.Parts)
	}
}
