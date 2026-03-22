package server

import (
	"context"
	"fmt"
	"io"
	"net"
	"testing"
	"time"

	"github.com/sahil-shubham/bhatti/pkg/engine"
)

// tunnelMockEngine extends mockEngine with a working Tunnel()
// that connects to a local TCP server.
type tunnelMockEngine struct {
	*mockEngine
	tunnelAddr string
}

func (m *tunnelMockEngine) Tunnel(_ context.Context, id string, port int) (io.ReadWriteCloser, error) {
	conn, err := net.DialTimeout("tcp", m.tunnelAddr, 2*time.Second)
	if err != nil {
		return nil, err
	}
	return conn, nil
}

func TestProxyManagerForwardAndStop(t *testing.T) {
	mockServer, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer mockServer.Close()

	go func() {
		for {
			conn, err := mockServer.Accept()
			if err != nil {
				return
			}
			go func(c net.Conn) {
				defer c.Close()
				c.Write([]byte("hello from container"))
			}(conn)
		}
	}()

	eng := &tunnelMockEngine{
		mockEngine: newMockEngine(),
		tunnelAddr: mockServer.Addr().String(),
	}
	pm := NewProxyManager(eng)

	entry, err := pm.Forward("sb1", "engine-id-1", 4321)
	if err != nil {
		t.Fatal(err)
	}
	if entry.HostPort == 0 {
		t.Fatal("expected non-zero host port")
	}
	if entry.SandboxID != "sb1" {
		t.Fatalf("expected sb1, got %s", entry.SandboxID)
	}

	conn, err := net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", entry.HostPort), 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	buf := make([]byte, 100)
	n, err := conn.Read(buf)
	if err != nil && err != io.EOF {
		t.Fatal(err)
	}
	if got := string(buf[:n]); got != "hello from container" {
		t.Fatalf("expected 'hello from container', got %q", got)
	}
	conn.Close()

	// Idempotent
	entry2, err := pm.Forward("sb1", "engine-id-1", 4321)
	if err != nil {
		t.Fatal(err)
	}
	if entry2.HostPort != entry.HostPort {
		t.Fatal("expected same host port")
	}

	fwds := pm.ActiveForwards("sb1")
	if len(fwds) != 1 {
		t.Fatalf("expected 1, got %d", len(fwds))
	}

	all := pm.AllForwards()
	if len(all) != 1 {
		t.Fatalf("expected 1, got %d", len(all))
	}

	pm.StopAll("sb1")
	fwds = pm.ActiveForwards("sb1")
	if len(fwds) != 0 {
		t.Fatal("expected 0 after StopAll")
	}
}

func TestProxyManagerStopForward(t *testing.T) {
	eng := &tunnelMockEngine{mockEngine: newMockEngine(), tunnelAddr: "127.0.0.1:1"}
	pm := NewProxyManager(eng)

	pm.Forward("sb1", "eid1", 3000)
	pm.Forward("sb1", "eid1", 3001)

	fwds := pm.ActiveForwards("sb1")
	if len(fwds) != 2 {
		t.Fatalf("expected 2, got %d", len(fwds))
	}

	pm.StopForward("sb1", 3000)
	fwds = pm.ActiveForwards("sb1")
	if len(fwds) != 1 {
		t.Fatalf("expected 1, got %d", len(fwds))
	}
}

func TestProxyManagerMultipleSandboxes(t *testing.T) {
	eng := &tunnelMockEngine{mockEngine: newMockEngine(), tunnelAddr: "127.0.0.1:1"}
	pm := NewProxyManager(eng)

	pm.Forward("sb1", "eid1", 3000)
	pm.Forward("sb2", "eid2", 3000)

	all := pm.AllForwards()
	if len(all) != 2 {
		t.Fatalf("expected 2, got %d", len(all))
	}

	pm.StopAll("sb1")
	all = pm.AllForwards()
	if len(all) != 1 {
		t.Fatalf("expected 1, got %d", len(all))
	}
}

func TestProxyManagerEmptyForwards(t *testing.T) {
	eng := &tunnelMockEngine{mockEngine: newMockEngine(), tunnelAddr: "127.0.0.1:1"}
	pm := NewProxyManager(eng)

	if len(pm.ActiveForwards("x")) != 0 {
		t.Fatal("expected 0")
	}
	if len(pm.AllForwards()) != 0 {
		t.Fatal("expected 0")
	}
	pm.StopAll("x")
	pm.StopForward("x", 8080)
}

var _ engine.Engine = (*tunnelMockEngine)(nil)
