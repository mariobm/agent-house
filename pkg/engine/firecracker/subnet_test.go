//go:build linux

package firecracker

import (
	"testing"
)

func TestSubnetFromIndex(t *testing.T) {
	tests := []struct {
		index   int
		gateway string
		subnet  string
		bridge  string
	}{
		{1, "10.0.1.1", "10.0.1.0/24", "brbhatti-1"},
		{2, "10.0.2.1", "10.0.2.0/24", "brbhatti-2"},
		{254, "10.0.254.1", "10.0.254.0/24", "brbhatti-254"},
		{255, "10.1.0.1", "10.1.0.0/24", "brbhatti-255"},     // wraps to hi=1
		{256, "10.1.1.1", "10.1.1.0/24", "brbhatti-256"},
		{508, "10.1.253.1", "10.1.253.0/24", "brbhatti-508"},
		{509, "10.1.254.1", "10.1.254.0/24", "brbhatti-509"},
		{510, "10.2.0.1", "10.2.0.0/24", "brbhatti-510"},     // wraps to hi=2
		{65024, "10.255.254.1", "10.255.254.0/24", "brbhatti-65024"}, // max
	}

	for _, tt := range tests {
		gateway, subnet, bridge := subnetFromIndex(tt.index)
		if gateway != tt.gateway {
			t.Errorf("index %d: gateway = %q, want %q", tt.index, gateway, tt.gateway)
		}
		if subnet != tt.subnet {
			t.Errorf("index %d: subnet = %q, want %q", tt.index, subnet, tt.subnet)
		}
		if bridge != tt.bridge {
			t.Errorf("index %d: bridge = %q, want %q", tt.index, bridge, tt.bridge)
		}
	}
}

func TestIPPoolAllocateRelease(t *testing.T) {
	pool := newIPPool("10.0.1.1")

	// First allocation should be .2
	ip, err := pool.Allocate()
	if err != nil {
		t.Fatal(err)
	}
	if ip != "10.0.1.2" {
		t.Fatalf("expected 10.0.1.2, got %s", ip)
	}

	// Second should be .3
	ip2, _ := pool.Allocate()
	if ip2 != "10.0.1.3" {
		t.Fatalf("expected 10.0.1.3, got %s", ip2)
	}

	// Release .2 and re-allocate
	pool.Release("10.0.1.2")
	ip3, _ := pool.Allocate()
	if ip3 != "10.0.1.2" {
		t.Fatalf("expected 10.0.1.2 after release, got %s", ip3)
	}

	// Mark should reserve
	pool.Mark("10.0.1.100")
	for i := 0; i < 250; i++ {
		ip, err := pool.Allocate()
		if err != nil {
			break // pool exhausted, expected
		}
		if ip == "10.0.1.100" {
			t.Fatal("allocated marked IP 10.0.1.100")
		}
	}
}

func TestIPPoolExhaustion(t *testing.T) {
	pool := newIPPool("10.0.1.1")

	// Allocate all 253 IPs
	for i := 0; i < 253; i++ {
		_, err := pool.Allocate()
		if err != nil {
			t.Fatalf("exhausted at %d (expected 253)", i)
		}
	}

	// Next should fail
	_, err := pool.Allocate()
	if err == nil {
		t.Fatal("expected exhaustion error")
	}
}
