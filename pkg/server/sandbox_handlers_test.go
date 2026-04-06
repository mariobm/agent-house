package server

import (
	"io"
	"testing"

	"github.com/sahil-shubham/bhatti/pkg/store"
)

// ==========================================================================
// keep_hot
// ==========================================================================

func TestKeepHotOnCreate(t *testing.T) {
	_, ts := setup(t)

	// Create with keep_hot: true
	resp := doReq(t, ts, "POST", "/sandboxes", map[string]any{
		"name":     uniqueName(t, "hot"),
		"keep_hot": true,
	})
	if resp.StatusCode != 201 {
		body, _ := io.ReadAll(resp.Body)
		t.Fatalf("expected 201, got %d: %s", resp.StatusCode, body)
	}
	var sb store.Sandbox
	decodeJSON(t, resp, &sb)
	t.Cleanup(func() { doReq(t, ts, "DELETE", "/sandboxes/"+sb.ID, nil) })

	if !sb.KeepHot {
		t.Fatal("expected keep_hot=true on created sandbox")
	}

	// GET should reflect it
	resp = doReq(t, ts, "GET", "/sandboxes/"+sb.ID, nil)
	var sb2 store.Sandbox
	decodeJSON(t, resp, &sb2)
	if !sb2.KeepHot {
		t.Fatal("expected keep_hot=true on GET")
	}
}

func TestKeepHotDefaultFalse(t *testing.T) {
	_, ts := setup(t)

	resp := doReq(t, ts, "POST", "/sandboxes", map[string]any{
		"name": uniqueName(t, "cold"),
	})
	var sb store.Sandbox
	decodeJSON(t, resp, &sb)
	t.Cleanup(func() { doReq(t, ts, "DELETE", "/sandboxes/"+sb.ID, nil) })

	if sb.KeepHot {
		t.Fatal("expected keep_hot=false by default")
	}
}

func TestKeepHotPatch(t *testing.T) {
	_, ts := setup(t)

	// Create without keep_hot
	sb := createSandbox(t, ts, uniqueName(t, "patch"))
	if sb.KeepHot {
		t.Fatal("precondition: keep_hot should be false")
	}

	// PATCH to enable
	resp := doReq(t, ts, "PATCH", "/sandboxes/"+sb.ID, map[string]any{
		"keep_hot": true,
	})
	if resp.StatusCode != 200 {
		body, _ := io.ReadAll(resp.Body)
		t.Fatalf("PATCH enable: expected 200, got %d: %s", resp.StatusCode, body)
	}
	var updated store.Sandbox
	decodeJSON(t, resp, &updated)
	if !updated.KeepHot {
		t.Fatal("expected keep_hot=true after PATCH")
	}

	// PATCH to disable
	resp = doReq(t, ts, "PATCH", "/sandboxes/"+sb.ID, map[string]any{
		"keep_hot": false,
	})
	if resp.StatusCode != 200 {
		t.Fatalf("PATCH disable: expected 200, got %d", resp.StatusCode)
	}
	var disabled store.Sandbox
	decodeJSON(t, resp, &disabled)
	if disabled.KeepHot {
		t.Fatal("expected keep_hot=false after disable PATCH")
	}
}

func TestKeepHotPatchNonexistent(t *testing.T) {
	_, ts := setup(t)
	resp := doReq(t, ts, "PATCH", "/sandboxes/nonexistent", map[string]any{
		"keep_hot": true,
	})
	if resp.StatusCode != 404 {
		t.Fatalf("expected 404, got %d", resp.StatusCode)
	}
	resp.Body.Close()
}

func TestKeepHotThermalCycleSkip(t *testing.T) {
	srv, ts := setup(t)

	// Create two sandboxes
	sb1 := createSandbox(t, ts, uniqueName(t, "hot"))
	sb2 := createSandbox(t, ts, uniqueName(t, "cold"))

	// Enable keep_hot on sb1
	resp := doReq(t, ts, "PATCH", "/sandboxes/"+sb1.ID, map[string]any{"keep_hot": true})
	resp.Body.Close()

	// Verify keep_hot is persisted in store
	sbStored, _ := srv.store.GetSandboxByID(sb1.ID)
	if !sbStored.KeepHot {
		t.Fatal("keep_hot not persisted in store")
	}

	// Verify sb2 is NOT keep_hot
	sb2Stored, _ := srv.store.GetSandboxByID(sb2.ID)
	if sb2Stored.KeepHot {
		t.Fatal("sb2 should not be keep_hot")
	}
}
