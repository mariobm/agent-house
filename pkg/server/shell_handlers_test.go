package server

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"github.com/sahil-shubham/bhatti/pkg/store"
)

// setupShellTest creates a server, user, and sandbox for shell tests.
func setupShellTest(t *testing.T) (*Server, *httptest.Server, *store.Sandbox) {
	t.Helper()
	srv, ts := setup(t)

	// Create sandbox via API
	resp := doReq(t, ts, "POST", "/sandboxes", map[string]string{"name": "shell-test"})
	var sb store.Sandbox
	decodeJSON(t, resp, &sb)
	t.Cleanup(func() {
		doReq(t, ts, "DELETE", "/sandboxes/"+sb.ID, nil).Body.Close()
	})
	return srv, ts, &sb
}

func TestShellTokenGenerateAndRevoke(t *testing.T) {
	_, ts, sb := setupShellTest(t)

	// Generate token
	resp := doReq(t, ts, "POST", "/sandboxes/"+sb.ID+"/shell-token", nil)
	if resp.StatusCode != 200 {
		t.Fatalf("expected 200, got %d", resp.StatusCode)
	}
	var result map[string]string
	decodeJSON(t, resp, &result)
	if result["token"] == "" {
		t.Fatal("expected non-empty token")
	}
	if result["url"] == "" {
		t.Fatal("expected non-empty url")
	}
	if !strings.Contains(result["url"], "#token=") {
		t.Fatalf("URL should contain fragment: %s", result["url"])
	}
	if !strings.Contains(result["url"], "/_shell/"+sb.ID) {
		t.Fatalf("URL should contain sandbox ID: %s", result["url"])
	}

	// Rotate — generates a new token
	resp2 := doReq(t, ts, "POST", "/sandboxes/"+sb.ID+"/shell-token", nil)
	var result2 map[string]string
	decodeJSON(t, resp2, &result2)
	if result2["token"] == result["token"] {
		t.Fatal("rotation should produce a different token")
	}

	// Revoke
	resp3 := doReq(t, ts, "DELETE", "/sandboxes/"+sb.ID+"/shell-token", nil)
	if resp3.StatusCode != 204 {
		t.Fatalf("expected 204, got %d", resp3.StatusCode)
	}
	resp3.Body.Close()
}

func TestShellWSAuthSuccess(t *testing.T) {
	_, ts, sb := setupShellTest(t)

	// Generate token
	resp := doReq(t, ts, "POST", "/sandboxes/"+sb.ID+"/shell-token", nil)
	var result map[string]string
	decodeJSON(t, resp, &result)
	token := result["token"]

	// Connect WebSocket (no HTTP auth needed — this bypasses auth middleware)
	wsURL := "ws" + strings.TrimPrefix(ts.URL, "http") + "/_shell/" + sb.ID + "/ws"
	ws, _, err := websocket.DefaultDialer.Dial(wsURL, nil)
	if err != nil {
		t.Fatalf("ws dial: %v", err)
	}
	defer ws.Close()

	// Send auth
	ws.WriteJSON(map[string]string{"type": "auth", "token": token})

	// Should get "connected" back
	ws.SetReadDeadline(time.Now().Add(2 * time.Second))
	_, msg, err := ws.ReadMessage()
	if err != nil {
		t.Fatalf("ws read: %v", err)
	}
	var connected map[string]interface{}
	if err := json.Unmarshal(msg, &connected); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if connected["type"] != "connected" {
		t.Fatalf("expected type=connected, got %v", connected["type"])
	}
	if connected["sandbox"] != "shell-test" {
		t.Fatalf("expected sandbox=shell-test, got %v", connected["sandbox"])
	}

	// Should get terminal output (mock engine sends "$ ")
	_, termMsg, err := ws.ReadMessage()
	if err != nil {
		t.Fatalf("ws read terminal: %v", err)
	}
	if !strings.Contains(string(termMsg), "$") {
		t.Logf("terminal output: %q", termMsg)
	}
}

func TestShellWSBadToken(t *testing.T) {
	_, ts, sb := setupShellTest(t)

	// Generate a token but use the wrong one
	resp := doReq(t, ts, "POST", "/sandboxes/"+sb.ID+"/shell-token", nil)
	resp.Body.Close()

	wsURL := "ws" + strings.TrimPrefix(ts.URL, "http") + "/_shell/" + sb.ID + "/ws"
	ws, _, err := websocket.DefaultDialer.Dial(wsURL, nil)
	if err != nil {
		t.Fatalf("ws dial: %v", err)
	}
	defer ws.Close()

	ws.WriteJSON(map[string]string{"type": "auth", "token": "wrong-token"})

	ws.SetReadDeadline(time.Now().Add(2 * time.Second))
	_, msg, err := ws.ReadMessage()
	if err != nil {
		t.Fatalf("ws read: %v", err)
	}
	var errMsg map[string]string
	json.Unmarshal(msg, &errMsg)
	if errMsg["error"] != "unauthorized" {
		t.Fatalf("expected unauthorized, got %v", errMsg["error"])
	}
}

func TestShellWSNoToken(t *testing.T) {
	_, ts, sb := setupShellTest(t)

	// No shell token set at all
	wsURL := "ws" + strings.TrimPrefix(ts.URL, "http") + "/_shell/" + sb.ID + "/ws"
	ws, _, err := websocket.DefaultDialer.Dial(wsURL, nil)
	if err != nil {
		t.Fatalf("ws dial: %v", err)
	}
	defer ws.Close()

	ws.WriteJSON(map[string]string{"type": "auth", "token": "any-token"})

	ws.SetReadDeadline(time.Now().Add(2 * time.Second))
	_, msg, err := ws.ReadMessage()
	if err != nil {
		t.Fatalf("ws read: %v", err)
	}
	var errMsg map[string]string
	json.Unmarshal(msg, &errMsg)
	if errMsg["error"] != "unauthorized" {
		t.Fatalf("expected unauthorized, got %v", errMsg["error"])
	}
}

func TestShellWSNonexistentSandbox(t *testing.T) {
	_, ts, _ := setupShellTest(t)

	// Nonexistent sandbox — should still upgrade WebSocket (anti-enumeration)
	wsURL := "ws" + strings.TrimPrefix(ts.URL, "http") + "/_shell/sbx_doesnotexist/ws"
	ws, _, err := websocket.DefaultDialer.Dial(wsURL, nil)
	if err != nil {
		t.Fatalf("ws dial should succeed (anti-enumeration): %v", err)
	}
	defer ws.Close()

	ws.WriteJSON(map[string]string{"type": "auth", "token": "any-token"})

	ws.SetReadDeadline(time.Now().Add(2 * time.Second))
	_, msg, err := ws.ReadMessage()
	if err != nil {
		t.Fatalf("ws read: %v", err)
	}
	var errMsg map[string]string
	json.Unmarshal(msg, &errMsg)
	// Same error as bad token — can't distinguish nonexistent from unauthorized
	if errMsg["error"] != "unauthorized" {
		t.Fatalf("expected unauthorized, got %v", errMsg["error"])
	}
}

func TestShellHTMLServed(t *testing.T) {
	_, ts, sb := setupShellTest(t)

	// /_shell/:id should serve the HTML page without auth
	resp, err := http.Get(ts.URL + "/_shell/" + sb.ID)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("expected 200, got %d", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); !strings.HasPrefix(ct, "text/html") {
		t.Fatalf("expected text/html, got %s", ct)
	}
	// Security headers
	if resp.Header.Get("X-Frame-Options") != "DENY" {
		t.Fatal("missing X-Frame-Options: DENY")
	}
	if resp.Header.Get("Referrer-Policy") != "no-referrer" {
		t.Fatal("missing Referrer-Policy: no-referrer")
	}
	csp := resp.Header.Get("Content-Security-Policy")
	if !strings.Contains(csp, "frame-ancestors 'none'") {
		t.Fatalf("CSP missing frame-ancestors: %s", csp)
	}
	if strings.Contains(csp, "unsafe-eval") {
		t.Fatal("CSP should not contain unsafe-eval")
	}
}

func TestShellHTMLServedForAnyID(t *testing.T) {
	_, ts, _ := setupShellTest(t)

	// Nonexistent sandbox — HTML should still be served (anti-enumeration)
	resp, err := http.Get(ts.URL + "/_shell/sbx_nonexistent_12345")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Fatalf("expected 200 for any ID, got %d", resp.StatusCode)
	}
}

func TestShellSessionTracker(t *testing.T) {
	tracker := newShellSessionTracker(2)

	// Add within limit
	if !tracker.Add("sb1") {
		t.Fatal("first add should succeed")
	}
	if !tracker.Add("sb1") {
		t.Fatal("second add should succeed")
	}
	// At limit
	if tracker.Add("sb1") {
		t.Fatal("third add should fail (limit=2)")
	}
	// Different sandbox is independent
	if !tracker.Add("sb2") {
		t.Fatal("different sandbox should succeed")
	}

	// Remove frees a slot
	tracker.Remove("sb1")
	if !tracker.Add("sb1") {
		t.Fatal("add after remove should succeed")
	}

	// Done + DisconnectAll
	done1 := tracker.Done("sb1")
	done2 := tracker.Done("sb1")
	select {
	case <-done1:
		t.Fatal("done should not be closed yet")
	default:
	}

	tracker.DisconnectAll("sb1")
	select {
	case <-done1:
		// good
	default:
		t.Fatal("done1 should be closed after DisconnectAll")
	}
	select {
	case <-done2:
		// good
	default:
		t.Fatal("done2 should be closed after DisconnectAll")
	}

	// DisconnectAll on unknown sandbox is a no-op
	tracker.DisconnectAll("sb_unknown")
}

func TestShellWSRevokeDisconnects(t *testing.T) {
	_, ts, sb := setupShellTest(t)

	// Generate token
	resp := doReq(t, ts, "POST", "/sandboxes/"+sb.ID+"/shell-token", nil)
	var result map[string]string
	decodeJSON(t, resp, &result)
	token := result["token"]

	// Connect WebSocket
	wsURL := "ws" + strings.TrimPrefix(ts.URL, "http") + "/_shell/" + sb.ID + "/ws"
	ws, _, err := websocket.DefaultDialer.Dial(wsURL, nil)
	if err != nil {
		t.Fatalf("ws dial: %v", err)
	}
	defer ws.Close()

	// Authenticate
	ws.WriteJSON(map[string]string{"type": "auth", "token": token})
	ws.SetReadDeadline(time.Now().Add(2 * time.Second))
	_, msg, _ := ws.ReadMessage()
	var connected map[string]interface{}
	json.Unmarshal(msg, &connected)
	if connected["type"] != "connected" {
		t.Fatalf("expected connected, got %v", connected["type"])
	}

	// Revoke via API
	revokeResp := doReq(t, ts, "DELETE", "/sandboxes/"+sb.ID+"/shell-token", nil)
	revokeResp.Body.Close()
	if revokeResp.StatusCode != 204 {
		t.Fatalf("revoke: expected 204, got %d", revokeResp.StatusCode)
	}

	// WebSocket should close — reading should fail
	ws.SetReadDeadline(time.Now().Add(2 * time.Second))
	for {
		_, _, err := ws.ReadMessage()
		if err != nil {
			break // expected — connection was closed by revoke
		}
	}
}
