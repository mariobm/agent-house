package server

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"log/slog"
	"net/http"
	"path"
	"strings"
	"sync"
	"time"

	"github.com/sahil-shubham/bhatti/pkg/agent/proto"
	"github.com/sahil-shubham/bhatti/pkg/engine"
	"github.com/sahil-shubham/bhatti/pkg/store"
)

type contextKey string

const userContextKey contextKey = "user"

// ThermalEngine is optionally implemented by engines that support thermal management.
type ThermalEngine interface {
	EnsureHot(ctx context.Context, id string) error
	ThermalState(id string) string
	Pause(ctx context.Context, id string) error
	Activity(ctx context.Context, id string) (*proto.ActivityInfo, error)
}

// ThermalConfig controls automatic thermal transitions.
type ThermalConfig struct {
	WarmTimeout time.Duration // idle → warm (default 30s)
	ColdTimeout time.Duration // warm idle → cold (default 30min)
}

// Server is the HTTP API server.
type Server struct {
	engine       engine.Engine
	store        *store.Store
	mux          *http.ServeMux
	stopThermal  context.CancelFunc
	startTime    time.Time
	lastActivity sync.Map // engineID → time.Time — host-side activity cache
}

// touchActivity records that a sandbox was accessed via the API.
// The thermal manager checks this before querying the guest agent,
// avoiding a TCP connection per sandbox per thermal cycle.
func (s *Server) touchActivity(engineID string) {
	s.lastActivity.Store(engineID, time.Now())
}

// New creates a new API server.
func New(eng engine.Engine, st *store.Store) *Server {
	s := &Server{
		engine:    eng,
		store:     st,
		mux:       http.NewServeMux(),
		startTime: time.Now(),
	}
	s.routes()
	return s
}

// Close stops background goroutines (thermal manager).
func (s *Server) Close() {
	if s.stopThermal != nil {
		s.stopThermal()
	}
}

// StartThermalManager starts the background goroutine that transitions idle
// sandboxes through thermal states: hot → warm → cold.
func (s *Server) StartThermalManager(cfg ThermalConfig) {
	te, ok := s.engine.(ThermalEngine)
	if !ok {
		return // engine doesn't support thermal management
	}
	if cfg.WarmTimeout == 0 {
		cfg.WarmTimeout = 30 * time.Second
	}
	if cfg.ColdTimeout == 0 {
		cfg.ColdTimeout = 30 * time.Minute
	}

	ctx, cancel := context.WithCancel(context.Background())
	s.stopThermal = cancel

	go func() {
		ticker := time.NewTicker(10 * time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				s.runThermalCycle(te, cfg)
			}
		}
	}()
}

func (s *Server) runThermalCycle(te ThermalEngine, cfg ThermalConfig) {
	sandboxes, err := s.store.ListAllSandboxes()
	if err != nil {
		return
	}
	for _, sb := range sandboxes {
		if sb.Status != "running" {
			continue
		}

		thermal := te.ThermalState(sb.EngineID)
		if thermal == "cold" || thermal == "" {
			continue
		}

		// Fast path: check host-side activity cache. If the sandbox had
		// API activity within warmTimeout, skip the agent query entirely.
		// This avoids opening a TCP connection per sandbox per cycle.
		if ts, ok := s.lastActivity.Load(sb.EngineID); ok {
			if time.Since(ts.(time.Time)) < cfg.WarmTimeout {
				continue // definitely active, skip agent query
			}
		}

		// Slow path: ask the agent for authoritative activity info.
		actCtx, actCancel := context.WithTimeout(context.Background(), 5*time.Second)
		activity, err := te.Activity(actCtx, sb.EngineID)
		actCancel()
		if err != nil {
			continue
		}

		idle := time.Since(time.Unix(activity.LastActivityUnix, 0))

		if thermal == "hot" && idle > cfg.WarmTimeout && activity.AttachedSessions == 0 {
			if err := te.Pause(context.Background(), sb.EngineID); err != nil {
				slog.Warn("thermal pause failed", "sandbox", sb.Name, "error", err)
				continue
			}
			slog.Info("thermal transition", "sandbox", sb.Name, "from", "hot", "to", "warm", "idle", idle.Round(time.Second))
		}

		if thermal == "warm" && idle > cfg.ColdTimeout {
			if err := s.engine.Stop(context.Background(), sb.EngineID); err != nil {
				slog.Warn("thermal snapshot failed", "sandbox", sb.Name, "error", err)
				continue
			}
			s.saveVMState(sb.ID, sb.EngineID)
			slog.Info("thermal transition", "sandbox", sb.Name, "from", "warm", "to", "cold", "idle", idle.Round(time.Second))
		}
	}
}

// ensureHot transparently wakes a sandbox from warm or cold state.
// Also touches the host-side activity cache so the thermal manager
// knows this sandbox was recently accessed without querying the agent.
// Returns nil if the engine doesn't support thermal management.
func (s *Server) ensureHot(ctx context.Context, engineID string) error {
	s.touchActivity(engineID)
	te, ok := s.engine.(ThermalEngine)
	if !ok {
		return nil
	}
	return te.EnsureHot(ctx, engineID)
}


// ServeHTTP implements http.Handler.
func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	// Normalize path before any checks to prevent path confusion attacks
	cleanPath := path.Clean(r.URL.Path)

	// Unauthenticated endpoints (exact match only)
	if cleanPath == "/health" {
		s.mux.ServeHTTP(w, r)
		return
	}

	// Extract bearer token from Authorization header only.
	// No query parameter auth — eliminates token-in-URL leakage.
	authHeader := r.Header.Get("Authorization")
	token := strings.TrimPrefix(authHeader, "Bearer ")
	if token == "" || token == authHeader {
		writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "authorization required"})
		return
	}

	// Hash the incoming token and look up user by hash.
	hash := sha256Hex(token)
	user, err := s.store.GetUserByKeyHash(hash)
	if err != nil {
		writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "invalid api key"})
		return
	}

	// Attach user to request context
	ctx := context.WithValue(r.Context(), userContextKey, user)
	s.mux.ServeHTTP(w, r.WithContext(ctx))
}

func sha256Hex(s string) string {
	h := sha256.Sum256([]byte(s))
	return hex.EncodeToString(h[:])
}

// UserFromContext extracts the authenticated user from the request context.
func UserFromContext(ctx context.Context) *store.User {
	u, _ := ctx.Value(userContextKey).(*store.User)
	return u
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	if err := json.NewEncoder(w).Encode(v); err != nil {
		slog.Error("json encode failed", "error", err)
	}
}

func readJSON(r *http.Request, v any) error {
	r.Body = http.MaxBytesReader(nil, r.Body, 1<<20) // 1MB limit
	defer r.Body.Close()
	return json.NewDecoder(r.Body).Decode(v)
}

// errResp is a helper for error responses.
func errResp(w http.ResponseWriter, code int, msg string) {
	writeJSON(w, code, map[string]string{"error": msg})
}
