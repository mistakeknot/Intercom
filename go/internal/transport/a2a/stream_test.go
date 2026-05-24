package a2a

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

// sseEvent is one parsed SSE record. data is the raw JSON payload; callers
// decode it into the appropriate event-struct type per event.
type sseEvent struct {
	event string
	data  []byte
}

// sseReader is a stateful SSE event reader. It owns a single bufio.Scanner over
// the response body and a background goroutine that emits parsed records onto
// an out channel. Tests reuse one reader across multiple Read calls so that
// scanner state (read position, partial line buffering) persists.
//
// Pattern:
//
//	r := newSSEReader(t, resp.Body)
//	defer r.close()
//	events := r.read(2, time.Second) // first two events
//	events2 := r.read(1, time.Second) // third event later
type sseReader struct {
	out  chan sseEvent
	done chan struct{}
}

func newSSEReader(t *testing.T, body io.Reader) *sseReader {
	t.Helper()
	r := &sseReader{
		out:  make(chan sseEvent, 16),
		done: make(chan struct{}),
	}
	go func() {
		defer close(r.out)
		scanner := bufio.NewScanner(body)
		scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)
		var cur sseEvent
		for scanner.Scan() {
			select {
			case <-r.done:
				return
			default:
			}
			line := scanner.Text()
			switch {
			case line == "":
				if cur.event != "" || len(cur.data) > 0 {
					select {
					case r.out <- cur:
					case <-r.done:
						return
					}
					cur = sseEvent{}
				}
			case strings.HasPrefix(line, "event: "):
				cur.event = strings.TrimPrefix(line, "event: ")
			case strings.HasPrefix(line, "data: "):
				if len(cur.data) > 0 {
					cur.data = append(cur.data, '\n')
				}
				cur.data = append(cur.data, []byte(strings.TrimPrefix(line, "data: "))...)
			}
		}
	}()
	return r
}

// read collects up to max events, blocking up to timeout per event.
func (r *sseReader) read(max int, timeout time.Duration) []sseEvent {
	var events []sseEvent
	for len(events) < max {
		select {
		case evt, ok := <-r.out:
			if !ok {
				return events
			}
			events = append(events, evt)
		case <-time.After(timeout):
			return events
		}
	}
	return events
}

// close signals the scanner goroutine to stop. Idempotent.
func (r *sseReader) close() {
	select {
	case <-r.done:
	default:
		close(r.done)
	}
}

func TestMessagesStream_EmitsSubmittedThenWorking(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	ch, err := srv.Subscribe(ctx)
	if err != nil {
		t.Fatalf("Subscribe: %v", err)
	}
	// Drain inbound so the stream handoff doesn't block.
	go func() {
		for range ch {
		}
	}()

	body := SendMessageRequest{
		Message: Message{
			MessageID: "stream-msg-1",
			Role:      RoleUser,
			ContextID: "sylveste-ewy3.4.1.1",
			Parts:     []Part{{Text: "hi"}},
		},
	}
	buf, _ := json.Marshal(body)

	resp, err := http.Post(ts.URL+"/messages:stream", "application/json", bytes.NewReader(buf))
	if err != nil {
		t.Fatalf("POST /messages:stream: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); ct != "text/event-stream" {
		t.Fatalf("Content-Type = %q, want text/event-stream", ct)
	}

	reader := newSSEReader(t, resp.Body)
	defer reader.close()

	events := reader.read(2, time.Second)
	if len(events) < 2 {
		t.Fatalf("got %d events, want at least 2 (SUBMITTED + WORKING)", len(events))
	}

	for i, want := range []TaskState{TaskStateSubmitted, TaskStateWorking} {
		if events[i].event != string(StreamEventStatus) {
			t.Errorf("event[%d] kind = %q, want %s", i, events[i].event, StreamEventStatus)
		}
		var sue TaskStatusUpdateEvent
		if decErr := json.Unmarshal(events[i].data, &sue); decErr != nil {
			t.Fatalf("event[%d] decode: %v (data=%q)", i, decErr, events[i].data)
		}
		if sue.Status.State != want {
			t.Errorf("event[%d] State = %q, want %q", i, sue.Status.State, want)
		}
		if sue.Final {
			t.Errorf("event[%d] Final=true unexpectedly", i)
		}
	}

	// Drive the task to COMPLETED via the store; the stream should emit a
	// final event and the response body should EOF shortly after.
	tasks, _ := srv.Store().List("", 50)
	if len(tasks) == 0 {
		t.Fatal("Store has no tasks")
	}
	taskID := tasks[len(tasks)-1].ID
	if _, err := srv.Store().UpdateStatus(taskID, TaskStateCompleted); err != nil {
		t.Fatalf("UpdateStatus COMPLETED: %v", err)
	}

	final := reader.read(1, time.Second)
	if len(final) != 1 {
		t.Fatalf("got %d final events, want 1", len(final))
	}
	var fue TaskStatusUpdateEvent
	if decErr := json.Unmarshal(final[0].data, &fue); decErr != nil {
		t.Fatalf("final decode: %v", decErr)
	}
	if fue.Status.State != TaskStateCompleted {
		t.Errorf("final State = %q, want COMPLETED", fue.Status.State)
	}
	if !fue.Final {
		t.Error("final Final=false, want true")
	}
}

func TestTaskSubscribe_BackfillTerminalEmitsFinalAndCloses(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	// Create + immediately complete a task.
	task := srv.Store().Create("sylveste-bead-test")
	if _, err := srv.Store().UpdateStatus(task.ID, TaskStateCompleted); err != nil {
		t.Fatalf("UpdateStatus: %v", err)
	}

	resp, err := http.Get(ts.URL + "/tasks/" + task.ID + ":subscribe")
	if err != nil {
		t.Fatalf("GET :subscribe: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}

	reader := newSSEReader(t, resp.Body)
	defer reader.close()

	events := reader.read(1, time.Second)
	if len(events) != 1 {
		t.Fatalf("got %d events, want 1 (final backfill)", len(events))
	}
	var sue TaskStatusUpdateEvent
	if decErr := json.Unmarshal(events[0].data, &sue); decErr != nil {
		t.Fatalf("decode: %v", decErr)
	}
	if !sue.Final {
		t.Error("backfill Final=false, want true (task already terminal)")
	}
	if sue.Status.State != TaskStateCompleted {
		t.Errorf("State = %q, want COMPLETED", sue.Status.State)
	}
}

func TestTaskSubscribe_StreamsLiveTransitions(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	task := srv.Store().Create("sylveste-bead-test")
	if _, err := srv.Store().UpdateStatus(task.ID, TaskStateWorking); err != nil {
		t.Fatalf("UpdateStatus WORKING: %v", err)
	}

	resp, err := http.Get(ts.URL + "/tasks/" + task.ID + ":subscribe")
	if err != nil {
		t.Fatalf("GET :subscribe: %v", err)
	}
	defer resp.Body.Close()

	reader := newSSEReader(t, resp.Body)
	defer reader.close()

	// First event: backfill of current WORKING state.
	backfill := reader.read(1, time.Second)
	if len(backfill) != 1 {
		t.Fatalf("got %d backfill events, want 1", len(backfill))
	}
	var sue TaskStatusUpdateEvent
	_ = json.Unmarshal(backfill[0].data, &sue)
	if sue.Status.State != TaskStateWorking {
		t.Errorf("backfill State = %q, want WORKING", sue.Status.State)
	}

	// Now drive to COMPLETED; subscriber should see the terminal event.
	go func() {
		time.Sleep(20 * time.Millisecond)
		_, _ = srv.Store().UpdateStatus(task.ID, TaskStateCompleted)
	}()

	live := reader.read(1, time.Second)
	if len(live) != 1 {
		t.Fatalf("got %d live events, want 1 (COMPLETED)", len(live))
	}
	var lue TaskStatusUpdateEvent
	_ = json.Unmarshal(live[0].data, &lue)
	if lue.Status.State != TaskStateCompleted || !lue.Final {
		t.Errorf("live event State=%q Final=%v, want COMPLETED + Final=true", lue.Status.State, lue.Final)
	}
}

func TestTaskSubscribe_UnknownTaskReturns404(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	resp, err := http.Get(ts.URL + "/tasks/no-such-task:subscribe")
	if err != nil {
		t.Fatalf("GET :subscribe: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusNotFound {
		t.Errorf("status = %d, want 404", resp.StatusCode)
	}
}

func TestMessagesStream_MalformedBodyReturns400(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	resp, err := http.Post(ts.URL+"/messages:stream", "application/json", strings.NewReader("{}"))
	if err != nil {
		t.Fatalf("POST: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusBadRequest {
		t.Errorf("status = %d, want 400 (missing messageId)", resp.StatusCode)
	}
}

func TestAgentCard_AutoSetsStreamingCapability(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	resp, err := http.Get(ts.URL + "/.well-known/agent.json")
	if err != nil {
		t.Fatalf("GET agent.json: %v", err)
	}
	defer resp.Body.Close()
	var card AgentCard
	_ = json.NewDecoder(resp.Body).Decode(&card)
	if !card.Capabilities.Streaming {
		t.Error("AgentCard.Capabilities.Streaming = false; New() should auto-set true now that streaming is wired")
	}
}

func TestBrokerSubscribeUnsubscribe(t *testing.T) {
	b := NewBroker()
	taskID := "task-x"

	if got := b.SubscriberCount(taskID); got != 0 {
		t.Fatalf("initial count = %d, want 0", got)
	}

	sub := b.Subscribe(taskID)
	if got := b.SubscriberCount(taskID); got != 1 {
		t.Errorf("after subscribe count = %d, want 1", got)
	}

	b.Unsubscribe(taskID, sub)
	if got := b.SubscriberCount(taskID); got != 0 {
		t.Errorf("after unsubscribe count = %d, want 0", got)
	}

	// Channel should be closed after Unsubscribe.
	if _, ok := <-sub; ok {
		t.Error("channel not closed after Unsubscribe")
	}
}

func TestBrokerPublishFinalClosesAllSubscribers(t *testing.T) {
	b := NewBroker()
	taskID := "task-final"

	sub1 := b.Subscribe(taskID)
	sub2 := b.Subscribe(taskID)

	b.PublishFinal(taskID, StreamEvent{
		Kind: StreamEventStatus,
		Status: &TaskStatusUpdateEvent{
			TaskID: taskID,
			Status: TaskStatus{State: TaskStateCompleted},
			Final:  true,
		},
	})

	for i, sub := range []<-chan StreamEvent{sub1, sub2} {
		evt, ok := <-sub
		if !ok {
			t.Errorf("sub[%d] closed before receiving final event", i)
			continue
		}
		if evt.Status == nil || !evt.Status.Final {
			t.Errorf("sub[%d] event Final=false, want true", i)
		}
		if _, open := <-sub; open {
			t.Errorf("sub[%d] still open after final", i)
		}
	}

	// New subscribe on terminated task returns closed channel.
	late := b.Subscribe(taskID)
	if _, ok := <-late; ok {
		t.Error("late subscriber on terminated task got open channel")
	}
}

func TestBrokerPublishDropsOnSlowSubscriber(t *testing.T) {
	b := NewBroker()
	taskID := "task-slow"
	sub := b.Subscribe(taskID)

	// Fill the buffer.
	for i := 0; i < DefaultSubscriberBuffer; i++ {
		b.Publish(taskID, StreamEvent{Kind: StreamEventStatus, Status: &TaskStatusUpdateEvent{TaskID: taskID}})
	}

	// One more should drop silently (non-blocking).
	done := make(chan struct{})
	go func() {
		b.Publish(taskID, StreamEvent{Kind: StreamEventStatus, Status: &TaskStatusUpdateEvent{TaskID: taskID}})
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(100 * time.Millisecond):
		t.Fatal("Publish blocked on full subscriber buffer; should drop")
	}

	b.Unsubscribe(taskID, sub)
}

func TestStreamUntilFinal_ExitOnClientCancel(t *testing.T) {
	srv := newTestServer(t)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	task := srv.Store().Create("sylveste-bead-cancel")
	_, _ = srv.Store().UpdateStatus(task.ID, TaskStateWorking)

	ctx, cancel := context.WithCancel(context.Background())
	req, _ := http.NewRequestWithContext(ctx, http.MethodGet, ts.URL+"/tasks/"+task.ID+":subscribe", nil)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatalf("Do: %v", err)
	}

	// Read the backfill event then cancel the client context.
	reader := newSSEReader(t, resp.Body)
	_ = reader.read(1, time.Second)
	reader.close()

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		_, _ = io.Copy(io.Discard, resp.Body)
	}()

	cancel()
	resp.Body.Close()

	doneCh := make(chan struct{})
	go func() {
		wg.Wait()
		close(doneCh)
	}()

	select {
	case <-doneCh:
		// handler exited via context cancel — broker subscriber cleaned up
	case <-time.After(time.Second):
		t.Fatal("streaming handler did not exit within 1s of client cancel")
	}

	// The handler's defer Unsubscribe runs as the handler goroutine unwinds;
	// the HTTP framework signals the response body close to the client side
	// before that unwind completes in all schedulers (observed flaky in
	// single-CPU runs). Poll within a deadline to absorb the small window.
	deadline := time.Now().Add(time.Second)
	var got int
	for time.Now().Before(deadline) {
		got = srv.broker.SubscriberCount(task.ID)
		if got == 0 {
			break
		}
		time.Sleep(5 * time.Millisecond)
	}
	if got != 0 {
		t.Errorf("broker SubscriberCount = %d after client cancel, want 0 (defer Unsubscribe)", got)
	}
}

// Sanity: unused import guard for errors (used elsewhere in package); keep.
var _ = errors.New
