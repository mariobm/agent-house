//go:build linux

package firecracker

import (
	"testing"
)

// TestIPPoolReleaseUnallocated verifies that releasing an IP that was
// never allocated doesn't corrupt the pool.
// This is the unit-level test for Bug #4.
func TestIPPoolReleaseUnallocated(t *testing.T) {
	p := newIPPool("10.0.99.1")

	// Allocate .2
	ip1, err := p.Allocate()
	if err != nil {
		t.Fatal(err)
	}
	if ip1 != "10.0.99.2" {
		t.Fatalf("expected 10.0.99.2, got %s", ip1)
	}

	// Release .2 without having allocated it via TryAllocate first.
	// This simulates Bug #4: cleanup defer calls Release on an IP that
	// TryAllocate failed to allocate.
	p.Release("10.0.99.2")

	// .2 should now be free (this is correct — the release worked).
	// But in Bug #4, the IP was allocated by ANOTHER sandbox, and the
	// Release freed it from under them.

	// Allocate again — should get .2 back (pool thinks it's free)
	ip2, err := p.Allocate()
	if err != nil {
		t.Fatal(err)
	}
	if ip2 != "10.0.99.2" {
		t.Fatalf("expected 10.0.99.2 after release, got %s", ip2)
	}
}

// TestIPPoolTryAllocateFailDoesntCorrupt is the precise Bug #4 scenario:
// IP .2 is allocated by sandbox A. TryAllocate(.2) fails. If a buggy
// cleanup then calls Release(.2), sandbox A's IP is freed.
func TestIPPoolTryAllocateFailDoesntCorrupt(t *testing.T) {
	p := newIPPool("10.0.99.1")

	// Sandbox A allocates .2
	ipA, err := p.Allocate()
	if err != nil {
		t.Fatal(err)
	}
	if ipA != "10.0.99.2" {
		t.Fatalf("expected 10.0.99.2, got %s", ipA)
	}

	// Snapshot resume tries to TryAllocate .2 — fails (in use)
	err = p.TryAllocate("10.0.99.2")
	if err == nil {
		t.Fatal("TryAllocate should fail — .2 is in use by A")
	}

	// BUG #4 SCENARIO: the cleanup defer calls Release(.2) even though
	// TryAllocate failed. This frees A's IP.
	// In CURRENT buggy code, this happens. After the fix, the cleanup
	// should NOT call Release because guestIP is only set after success.
	//
	// Simulate the buggy behavior:
	p.Release("10.0.99.2") // THIS IS THE BUG — freeing an IP we don't own

	// Sandbox B allocates — gets .2 (double-allocated with A!)
	ipB, err := p.Allocate()
	if err != nil {
		t.Fatal(err)
	}
	if ipB == ipA {
		t.Fatalf("Bug #4 confirmed: sandbox B got A's IP %s after buggy Release.\n"+
			"Fix: only set guestIP after successful TryAllocate, so cleanup "+
			"defer doesn't Release an IP that was never allocated.", ipB)
	}
	// After fix, .2 should still be allocated to A, and B gets .3
	t.Logf("B got %s (A has %s) — no corruption", ipB, ipA)
}

// TestIPPoolTryAllocateSuccessThenRelease verifies the correct path:
// TryAllocate succeeds, then Release on cleanup properly frees it.
func TestIPPoolTryAllocateSuccessThenRelease(t *testing.T) {
	p := newIPPool("10.0.99.1")

	// TryAllocate .5 (nothing else using it)
	err := p.TryAllocate("10.0.99.5")
	if err != nil {
		t.Fatalf("TryAllocate .5: %v", err)
	}

	// Allocate sequentially — .5 should be skipped
	ip1, _ := p.Allocate()
	if ip1 != "10.0.99.2" {
		t.Fatalf("expected .2, got %s", ip1)
	}
	ip2, _ := p.Allocate()
	if ip2 != "10.0.99.3" {
		t.Fatalf("expected .3, got %s", ip2)
	}

	// Release .5 (cleanup after failed resume)
	p.Release("10.0.99.5")

	// Now .4 should be next, then .5 is available again
	ip3, _ := p.Allocate()
	if ip3 != "10.0.99.4" {
		t.Fatalf("expected .4, got %s", ip3)
	}
	ip4, _ := p.Allocate()
	if ip4 != "10.0.99.5" {
		t.Fatalf("expected .5 after release, got %s", ip4)
	}
}
