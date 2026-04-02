//go:build linux

package firecracker

import "sync"

// ringBuffer is a fixed-size circular buffer implementing io.Writer.
// Used to capture Firecracker stderr per-VM (bounded at maxSize regardless
// of how much the guest/FC writes).
type ringBuffer struct {
	mu   sync.Mutex
	buf  []byte
	size int // allocated size
	w    int // next write position
	full bool
}

func newRingBuffer(size int) *ringBuffer {
	return &ringBuffer{
		buf:  make([]byte, size),
		size: size,
	}
}

func (r *ringBuffer) Write(p []byte) (int, error) {
	r.mu.Lock()
	defer r.mu.Unlock()

	n := len(p)
	if n >= r.size {
		// Data larger than buffer — keep only the last r.size bytes
		copy(r.buf, p[n-r.size:])
		r.w = 0
		r.full = true
		return n, nil
	}

	// How much fits before wrap?
	space := r.size - r.w
	if n <= space {
		copy(r.buf[r.w:], p)
		r.w += n
		if r.w == r.size {
			r.w = 0
			r.full = true
		}
	} else {
		copy(r.buf[r.w:], p[:space])
		copy(r.buf, p[space:])
		r.w = n - space
		r.full = true
	}
	return n, nil
}

func (r *ringBuffer) String() string {
	r.mu.Lock()
	defer r.mu.Unlock()

	if !r.full {
		return string(r.buf[:r.w])
	}
	// Buffer has wrapped: data from w..end + 0..w
	result := make([]byte, r.size)
	n := copy(result, r.buf[r.w:])
	copy(result[n:], r.buf[:r.w])
	return string(result)
}
