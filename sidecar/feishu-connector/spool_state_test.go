package main

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net"
	"os"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

type repeatedMergeFetcher struct {
	calls   atomic.Int32
	release chan struct{}
}

func (f *repeatedMergeFetcher) Fetch(ctx context.Context, _ appCfg, _ string) (string, []resourceOut) {
	if f.calls.Add(1) > 1 {
		select {
		case <-f.release:
		case <-ctx.Done():
		}
	}
	return "complete forwarded body", nil
}

func TestDuplicateMergeCallbackKeepsEnrichedSpool(t *testing.T) {
	spool := newMergeForwardSpool(t.TempDir())
	fetcher := &repeatedMergeFetcher{release: make(chan struct{})}
	defer close(fetcher.release)
	slots := make(chan struct{}, 2)
	handler := buildEventDispatcherWithMergeForward(
		appCfg{AppID: "cli_test"}, &sink{}, fetcher, slots, spool,
	)
	if _, err := handler.Do(context.Background(), mergeForwardEventPayload()); err != nil {
		t.Fatal(err)
	}
	path, err := spool.eventPath(outEvent{AppID: "cli_test", MessageID: "om_merge"})
	if err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(time.Second)
	for {
		var event outEvent
		body, _ := os.ReadFile(path)
		_ = json.Unmarshal(body, &event)
		if event.Text == "complete forwarded body" && len(slots) == 0 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("first callback did not persist enriched body")
		}
		time.Sleep(time.Millisecond)
	}
	if _, err := handler.Do(context.Background(), mergeForwardEventPayload()); err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var event outEvent
	if err := json.Unmarshal(body, &event); err != nil {
		t.Fatal(err)
	}
	if event.Text != "complete forwarded body" {
		t.Fatalf("duplicate callback replaced enriched spool with raw event: %q", event.Text)
	}
	if len(slots) != 0 {
		t.Fatal("duplicate callback acquired a fetch slot")
	}
	if calls := fetcher.calls.Load(); calls != 1 {
		t.Fatalf("duplicate fetch owners: %d", calls)
	}
}

func TestMergeForwardOwnershipTransitions(t *testing.T) {
	for _, operation := range []string{"enrich", "transfer"} {
		t.Run(operation, func(t *testing.T) {
			spool := newMergeForwardSpool(t.TempDir())
			event := outEvent{Type: "message", AppID: "app", MessageID: "message"}
			pending, err := spool.begin(event, nil)
			if err != nil {
				t.Fatal(err)
			}
			ready, err := pending.enrich("enriched", nil)
			if err != nil {
				t.Fatal(err)
			}
			replacement := ready.record.event
			replacement.Text = "replacement"
			body, _ := json.Marshal(replacement)
			if err := os.WriteFile(ready.record.path, body, 0o600); err != nil {
				t.Fatal(err)
			}
			conn := &recordingConn{}
			if operation == "enrich" {
				if _, err := pending.enrich("stale", nil); err == nil {
					t.Fatal("stale fetch replaced newer body")
				}
			} else {
				if err := ready.transfer(&sink{conn: conn}); err == nil {
					t.Fatal("stale transfer removed newer body")
				}
			}
			actual, err := os.ReadFile(ready.record.path)
			if err != nil || string(actual) != string(body) {
				t.Fatalf("replacement lost: %s %v", actual, err)
			}
			if conn.writeCount() != 0 {
				t.Fatal("stale owner sent an event")
			}
		})
	}
}

func TestMergeForwardTransferKeepsRecordOnSocketFailureAndClosesOnce(t *testing.T) {
	spool := newMergeForwardSpool(t.TempDir())
	pending, err := spool.begin(outEvent{Type: "message", AppID: "app", MessageID: "message"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	ready, err := pending.enrich("body", nil)
	if err != nil {
		t.Fatal(err)
	}
	if err := ready.transfer(&sink{}); err == nil {
		t.Fatal("disconnected sink accepted transfer")
	}
	if _, err := os.Stat(ready.record.path); err != nil {
		t.Fatal("failed transfer lost record", err)
	}
	conn := &recordingConn{}
	if err := ready.transfer(&sink{conn: conn}); err != nil {
		t.Fatal(err)
	}
	if err := ready.transfer(&sink{conn: conn}); err == nil {
		t.Fatal("released owner transferred again")
	}
	if conn.writeCount() != 1 {
		t.Fatalf("duplicate writes: %d", conn.writeCount())
	}
	if _, err := os.Stat(ready.record.path); !os.IsNotExist(err) {
		t.Fatal("successful socket transfer retained record", err)
	}
}

func TestMergeForwardEnrichmentFailurePreservesRawRecord(t *testing.T) {
	spool := newMergeForwardSpool(t.TempDir())
	pending, err := spool.begin(outEvent{Type: "message", AppID: "app", MessageID: "message"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	// The atomic writer cannot create its temporary file at a directory.
	if err := os.Mkdir(pending.record.path+".tmp", 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := pending.enrich("body", nil); err == nil {
		t.Fatal("expected enriched persistence failure")
	}
	actual, err := os.ReadFile(pending.record.path)
	if err != nil || string(actual) != string(pending.record.body) {
		t.Fatalf("raw record lost: %s %v", actual, err)
	}
}

func TestMergeForwardDrainRecoversLegacyRawAndEnrichedRecords(t *testing.T) {
	for _, text := range []string{"", "enriched"} {
		t.Run(text, func(t *testing.T) {
			spool := newMergeForwardSpool(t.TempDir())
			event := outEvent{Type: "message", AppID: "app", MessageID: "message", Text: text}
			path, err := spool.eventPath(event)
			if err != nil {
				t.Fatal(err)
			}
			body, _ := json.MarshalIndent(event, "", "  ")
			if err := os.WriteFile(path, body, 0o600); err != nil {
				t.Fatal(err)
			}
			if err := spool.drain(&sink{}); err == nil {
				t.Fatal("disconnected drain succeeded")
			}
			if _, err := os.Stat(path); err != nil {
				t.Fatal("drain failure removed record", err)
			}
			conn := &recordingConn{}
			if err := spool.drain(&sink{conn: conn}); err != nil {
				t.Fatal(err)
			}
			var sent outEvent
			if err := json.Unmarshal(conn.lastWrite(), &sent); err != nil {
				t.Fatal(err)
			}
			if sent.AppID != event.AppID || sent.MessageID != event.MessageID {
				t.Fatal("recovery changed identity")
			}
			if text != "" && sent.Text != text {
				t.Fatal("recovery changed enriched text")
			}
			if text == "" && sent.Text == "" {
				t.Fatal("interrupted fetch lacks visible fallback")
			}
			if _, err := os.Stat(path); !os.IsNotExist(err) {
				t.Fatal("recovery did not remove transferred record", err)
			}
		})
	}
}

func TestMergeForwardBeginRejectsCorruptionAndIdentityCollision(t *testing.T) {
	for _, body := range []string{`{broken`, `{"type":"message","app_id":"other","message_id":"message","sender":"","text":"","chat_id":""}`} {
		spool := newMergeForwardSpool(t.TempDir())
		event := outEvent{Type: "message", AppID: "app", MessageID: "message"}
		path, _ := spool.eventPath(event)
		if err := os.WriteFile(path, []byte(body), 0o600); err != nil {
			t.Fatal(err)
		}
		if _, err := spool.begin(event, nil); err == nil {
			t.Fatal("invalid persisted owner was acknowledged")
		}
		actual, _ := os.ReadFile(path)
		if string(actual) != body {
			t.Fatal("invalid record overwritten")
		}
	}
}

type replacingSpoolConn struct {
	recordingConn
	replace func()
}

func (c *replacingSpoolConn) Write(body []byte) (int, error) {
	n, err := c.recordingConn.Write(body)
	c.replace()
	return n, err
}

func TestMergeForwardTransferKeepsReplacementWrittenDuringSend(t *testing.T) {
	spool := newMergeForwardSpool(t.TempDir())
	pending, err := spool.begin(outEvent{Type: "message", AppID: "app", MessageID: "message"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	ready, err := pending.enrich("body", nil)
	if err != nil {
		t.Fatal(err)
	}
	replacement := ready.record.event
	replacement.Text = "newer body"
	body, _ := json.Marshal(replacement)
	conn := &replacingSpoolConn{replace: func() {
		if err := os.WriteFile(ready.record.path, body, 0o600); err != nil {
			t.Fatal(err)
		}
	}}
	if err := ready.transfer(&sink{conn: conn}); err == nil {
		t.Fatal("stale transfer removed replacement")
	}
	actual, err := os.ReadFile(ready.record.path)
	if err != nil || string(actual) != string(body) {
		t.Fatalf("replacement lost: %s %v", actual, err)
	}
}

func TestDuplicateMergeCallbackDoesNotOvertakeBlockedFetchAtCapacity(t *testing.T) {
	spool := newMergeForwardSpool(t.TempDir())
	fetcher := &blockingMergeFetcher{started: make(chan struct{}), release: make(chan struct{})}
	slots := make(chan struct{}, 1)
	release := sync.OnceFunc(func() { close(fetcher.release) })
	defer func() {
		release()
		deadline := time.Now().Add(time.Second)
		for len(slots) != 0 && time.Now().Before(deadline) {
			time.Sleep(time.Millisecond)
		}
	}()
	conn := &recordingConn{}
	handler := buildEventDispatcherWithMergeForward(appCfg{AppID: "cli_test"}, &sink{conn: conn}, fetcher, slots, spool)
	if _, err := handler.Do(context.Background(), mergeForwardEventPayload()); err != nil {
		t.Fatal(err)
	}
	select {
	case <-fetcher.started:
	case <-time.After(time.Second):
		t.Fatal("fetch did not start")
	}
	if _, err := handler.Do(context.Background(), mergeForwardEventPayload()); err != nil {
		t.Fatal(err)
	}
	if conn.writeCount() != 0 {
		t.Fatal("duplicate busy placeholder overtook pending forwarded body")
	}
	release()
	deadline := time.Now().Add(time.Second)
	for len(slots) != 0 && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if len(slots) != 0 || conn.writeCount() != 1 {
		t.Fatal("forwarded result was not delivered exactly once")
	}
	var event outEvent
	_ = json.Unmarshal(conn.lastWrite(), &event)
	if event.Text != "异步转发正文" {
		t.Fatalf("unexpected final body: %q", event.Text)
	}
}

type notifyingSpoolConn struct {
	net.Conn
	started chan struct{}
}

func (c *notifyingSpoolConn) Write(body []byte) (int, error) {
	close(c.started)
	return c.Conn.Write(body)
}

func TestBlockedMergeTransferDoesNotBlockOtherPersistence(t *testing.T) {
	spool := newMergeForwardSpool(t.TempDir())
	pending, err := spool.begin(outEvent{Type: "message", AppID: "app", MessageID: "first"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	ready, err := pending.enrich("body", nil)
	if err != nil {
		t.Fatal(err)
	}
	client, server := net.Pipe()
	started := make(chan struct{})
	finished := make(chan error, 1)
	done := make(chan struct{})
	defer func() {
		client.Close()
		server.Close()
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Error("transfer worker did not stop")
		}
	}()
	go func() {
		defer close(done)
		finished <- ready.transfer(&sink{conn: &notifyingSpoolConn{client, started}})
	}()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("transfer did not reach socket write")
	}
	second := make(chan error, 1)
	go func() {
		_, err := spool.begin(outEvent{Type: "message", AppID: "app", MessageID: "second"}, nil)
		second <- err
	}()
	select {
	case err := <-second:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("network backpressure held the global spool lock")
	}
	if err := ready.transfer(&sink{conn: &recordingConn{}}); err == nil {
		t.Fatal("a second sender acquired the same record")
	}
	go io.Copy(io.Discard, server)
	select {
	case err := <-finished:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("transfer did not finish")
	}
}

func TestMergeForwardAdmissionReleasesFailedSlotsAndCoalescesAtCapacity(t *testing.T) {
	spool := newMergeForwardSpool(t.TempDir())
	slots := make(chan struct{}, 1)
	first := outEvent{Type: "message", AppID: "app", MessageID: "first"}
	pending, err := spool.begin(first, slots)
	if err != nil || pending == nil {
		t.Fatal("new owner not admitted", err)
	}
	duplicate, err := spool.begin(first, slots)
	if err != nil || duplicate != nil || len(slots) != 1 {
		t.Fatal("duplicate consumed capacity", err)
	}
	if _, err := spool.begin(outEvent{Type: "message", AppID: "app", MessageID: "second"}, slots); !errors.Is(err, errMergeForwardBusy) {
		t.Fatal("new message should respect capacity", err)
	}
	<-slots
	second := outEvent{Type: "message", AppID: "app", MessageID: "second"}
	path, _ := spool.eventPath(second)
	if err := os.Mkdir(path+".tmp", 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := spool.begin(second, slots); err == nil || len(slots) != 0 {
		t.Fatal("failed persistence kept fetch slot", err)
	}
}
