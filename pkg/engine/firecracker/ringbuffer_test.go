//go:build linux

package firecracker

import (
	"sync"
	"testing"
)

func TestRingBufferBasic(t *testing.T) {
	rb := newRingBuffer(16)
	rb.Write([]byte("hello"))
	if got := rb.String(); got != "hello" {
		t.Errorf("got %q, want %q", got, "hello")
	}
}

func TestRingBufferExactFill(t *testing.T) {
	rb := newRingBuffer(5)
	rb.Write([]byte("12345"))
	if got := rb.String(); got != "12345" {
		t.Errorf("got %q, want %q", got, "12345")
	}
}

func TestRingBufferOverflow(t *testing.T) {
	rb := newRingBuffer(8)
	rb.Write([]byte("hello"))    // 5 bytes
	rb.Write([]byte(" world!"))  // 7 bytes, total 12, wraps
	got := rb.String()
	// Should contain the last 8 bytes: "o world!" or similar
	if len(got) != 8 {
		t.Errorf("expected 8 bytes, got %d: %q", len(got), got)
	}
	// The last 8 bytes of "hello world!" is "world!\x00\x00" — no.
	// "hello" + " world!" = "hello world!" (12 bytes), last 8 = "world!"
	// Wait: "hello" is 5, " world!" is 7. Buffer is 8.
	// After "hello": buf = "hello\0\0\0", w=5, full=false
	// Write " world!" (7 bytes): space=8-5=3, fits 3 then wraps.
	// buf[5..8] = " wo", buf[0..4] = "rld!", w=4, full=true
	// String: buf[4..8] + buf[0..4] = "o wo" + "rld!" = "o world!"
	// Hmm, let me just check it contains "world"
	if got != "o world!" {
		t.Errorf("got %q, want %q", got, "o world!")
	}
}

func TestRingBufferLargeWrite(t *testing.T) {
	rb := newRingBuffer(4)
	rb.Write([]byte("abcdefgh")) // 8 bytes into 4-byte buffer
	got := rb.String()
	if got != "efgh" {
		t.Errorf("got %q, want %q", got, "efgh")
	}
}

func TestRingBufferEmpty(t *testing.T) {
	rb := newRingBuffer(16)
	if got := rb.String(); got != "" {
		t.Errorf("expected empty, got %q", got)
	}
}

func TestRingBufferConcurrent(t *testing.T) {
	rb := newRingBuffer(1024)
	var wg sync.WaitGroup
	for i := 0; i < 100; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			rb.Write([]byte("concurrent write test data\n"))
		}()
	}
	wg.Wait()
	// Should not panic, and should have some data
	got := rb.String()
	if len(got) == 0 {
		t.Error("expected non-empty after concurrent writes")
	}
}
