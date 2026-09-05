package proto

import (
	"bytes"
	"io"
	"testing"
)

func BenchmarkWriteFrame1KB(b *testing.B) {
	payload := make([]byte, 1024)
	var buf bytes.Buffer
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		buf.Reset()
		WriteFrame(&buf, STDOUT, payload)
	}
}

func BenchmarkWriteFrame8KB(b *testing.B) {
	payload := make([]byte, 8192)
	var buf bytes.Buffer
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		buf.Reset()
		WriteFrame(&buf, STDOUT, payload)
	}
}

func BenchmarkWriteFrame32KB(b *testing.B) {
	payload := make([]byte, 32768)
	var buf bytes.Buffer
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		buf.Reset()
		WriteFrame(&buf, STDOUT, payload)
	}
}

func BenchmarkReadFrame1KB(b *testing.B) {
	payload := make([]byte, 1024)
	var buf bytes.Buffer
	WriteFrame(&buf, STDOUT, payload)
	frame := buf.Bytes()

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		ReadFrame(bytes.NewReader(frame))
	}
}

func BenchmarkReadFrame32KB(b *testing.B) {
	payload := make([]byte, 32768)
	var buf bytes.Buffer
	WriteFrame(&buf, STDOUT, payload)
	frame := buf.Bytes()

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		ReadFrame(bytes.NewReader(frame))
	}
}

func BenchmarkFrameRoundTrip1KB(b *testing.B) {
	payload := make([]byte, 1024)
	pr, pw := io.Pipe()

	go func() {
		for {
			if err := WriteFrame(pw, STDOUT, payload); err != nil {
				return
			}
		}
	}()

	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		_, _, err := ReadFrame(pr)
		if err != nil {
			b.Fatal(err)
		}
	}
}

func BenchmarkSendJSON(b *testing.B) {
	msg := ExecRequest{
		Argv: []string{"echo", "hello", "world"},
		Env:  map[string]string{"PATH": "/usr/bin", "HOME": "/root"},
	}
	var buf bytes.Buffer
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		buf.Reset()
		SendJSON(&buf, EXEC_REQ, msg)
	}
}
