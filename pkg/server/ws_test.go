package server

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"

	dockerengine "github.com/sahil-shubham/bhatti/pkg/engine/docker"
	"github.com/sahil-shubham/bhatti/pkg/store"
)

func TestWebSocketTerminal(t *testing.T) {
	skipIfNoDocker(t)
	ensureAlpinePulled(t)

	dir := t.TempDir()
	st, err := store.New(filepath.Join(dir, "test.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()

	eng, err := dockerengine.New()
	if err != nil {
		t.Fatal(err)
	}

	// Create a test user
	keyHash := sha256Hex("ws-test-token")
	st.CreateUser(store.User{
		ID: "usr_ws", Name: "ws-user", APIKeyHash: keyHash,
		MaxSandboxes: 50, MaxCPUsPerSandbox: 4, MaxMemoryMBPerSandbox: 4096,
		SubnetIndex: 1, CreatedAt: time.Now(),
	})

	srv := New(eng, st)
	ts := httptest.NewServer(srv)
	defer func() { srv.Close(); ts.Close() }()

	name := uniqueName(t, "ws")

	// Create template
	resp := doReqWSTest(t, ts, "POST", "/templates", map[string]any{
		"name":  "alpine",
		"image": "alpine:latest",
	})
	var tmpl store.Template
	decodeJSON(t, resp, &tmpl)

	// Create sandbox
	resp = doReqWSTest(t, ts, "POST", "/sandboxes", map[string]any{
		"template_id": tmpl.ID,
		"name":        name,
	})
	if resp.StatusCode != 201 {
		t.Fatalf("expected 201, got %d", resp.StatusCode)
	}
	var sb store.Sandbox
	decodeJSON(t, resp, &sb)
	defer doReqWSTest(t, ts, "DELETE", "/sandboxes/"+sb.ID, nil)

	// Connect WebSocket with auth header
	wsURL := "ws" + strings.TrimPrefix(ts.URL, "http") + "/sandboxes/" + sb.ID + "/ws"
	wsHeader := http.Header{}
	wsHeader.Set("Authorization", "Bearer ws-test-token")
	ws, _, err := websocket.DefaultDialer.Dial(wsURL, wsHeader)
	if err != nil {
		t.Fatal(err)
	}
	defer ws.Close()

	// Send a command
	if err := ws.WriteMessage(websocket.BinaryMessage, []byte("echo ws-works\n")); err != nil {
		t.Fatal(err)
	}

	// Read until we see our output
	deadline := time.After(5 * time.Second)
	var total string
	for {
		select {
		case <-deadline:
			t.Fatalf("timeout waiting for output. got so far: %q", total)
		default:
		}
		ws.SetReadDeadline(time.Now().Add(2 * time.Second))
		_, msg, err := ws.ReadMessage()
		if err != nil {
			t.Fatalf("ws read error: %v (got so far: %q)", err, total)
		}
		total += string(msg)
		if strings.Contains(total, "ws-works") {
			break
		}
	}

	// Test resize
	resizeMsg, _ := json.Marshal(map[string]any{
		"type": "resize",
		"rows": 40,
		"cols": 120,
	})
	if err := ws.WriteMessage(websocket.TextMessage, resizeMsg); err != nil {
		t.Fatal(err)
	}
	// No error means resize was accepted

	t.Logf("WebSocket terminal test passed, output: %q", total)
}

func doReqWSTest(t *testing.T, ts *httptest.Server, method, path string, body any) *http.Response {
	t.Helper()
	var bodyReader *strings.Reader
	if body != nil {
		b, _ := json.Marshal(body)
		bodyReader = strings.NewReader(string(b))
	}
	var req *http.Request
	if bodyReader != nil {
		req, _ = http.NewRequest(method, ts.URL+path, bodyReader)
	} else {
		req, _ = http.NewRequest(method, ts.URL+path, nil)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer ws-test-token")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	return resp
}
