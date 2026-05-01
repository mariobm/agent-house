//go:build linux

package main

import (
	"fmt"
	"os"
	"strings"
)

// buildStartGraph computes the activation order for a set of units,
// honouring After= directives. Returns groups of units; each group can
// be activated in parallel, but every group must complete (all units
// reach "active") before the next starts. This mirrors systemd's
// dependency resolution at boot, where multi-service stacks like
// postgres -> pgbouncer -> webapp need their dependencies up before
// they themselves come up.
//
// What's honoured in this cut (F3 minimum viable):
//
//   - After=B on A produces an edge B->A: B activates first.
//   - Before=B on A produces an edge A->B: A activates first (inverted).
//     Real systemd treats Before= as the dual of After=; we do too.
//   - Wants= and Requires= are NOT used for ordering; they only pull
//     additional units into the start set (handled by expandStartSet
//     before this function runs). Failure propagation for Requires=
//     is deferred to F3.5.
//
// Cycles: detected via Kahn's topological sort. If we can't drain the
// graph (some nodes have lingering in-degree > 0), the remaining nodes
// are returned as a single best-effort group with a warning. systemd
// does the same: a dependency cycle is a unit-file bug; we don't refuse
// to start the system over it.
//
// Missing or masked dependencies: After=nonexistent.service produces a
// dangling edge that resolves to a node not in the input set. We drop
// such edges before the sort \u2014 a missing dep can't gate anything we
// know about. This matches systemd's behaviour: missing-dep warnings
// log at boot but don't block.
func buildStartGraph(units []*Unit) [][]*Unit {
	if len(units) == 0 {
		return nil
	}

	// Index by canonical name for O(1) edge resolution. Two units with
	// the same canonical name (which shouldn't happen in practice but
	// could during alias-merge edge cases) collapse to one entry; the
	// last writer wins, matching the registry's identity model.
	byName := make(map[string]*Unit, len(units))
	for _, u := range units {
		byName[u.Canonical] = u
	}

	// Build the edge set: parents[child] = set of canonical names that
	// must activate before child. We use a map-of-set so duplicate
	// edges (e.g., the same dep in multiple After= lines) collapse.
	parents := make(map[string]map[string]bool, len(units))
	for _, u := range units {
		parents[u.Canonical] = map[string]bool{}
	}
	for _, u := range units {
		// After=A means: u activates after A.
		for _, raw := range u.Sections.getAll("Unit", "After") {
			for _, dep := range parseDepList(raw) {
				if dep == u.Canonical {
					continue // self-edge: log and ignore
				}
				if _, ok := byName[dep]; !ok {
					continue // dep not in our start set; drop the edge
				}
				parents[u.Canonical][dep] = true
			}
		}
		// Before=A means: u activates before A. Equivalent to A having
		// After=u. Add the inverse edge.
		for _, raw := range u.Sections.getAll("Unit", "Before") {
			for _, dep := range parseDepList(raw) {
				if dep == u.Canonical {
					continue
				}
				if _, ok := byName[dep]; !ok {
					continue
				}
				if parents[dep] == nil {
					parents[dep] = map[string]bool{}
				}
				parents[dep][u.Canonical] = true
			}
		}
	}

	// Kahn's algorithm: each round, take all nodes with no unsatisfied
	// parents (= already-activated dependencies). Append them as one
	// parallel group. Then "remove" them by deleting them from every
	// other node's parent set.
	var groups [][]*Unit
	remaining := make(map[string]bool, len(units))
	for _, u := range units {
		remaining[u.Canonical] = true
	}

	for len(remaining) > 0 {
		var ready []*Unit
		for name := range remaining {
			if len(parents[name]) == 0 {
				ready = append(ready, byName[name])
			}
		}
		if len(ready) == 0 {
			// Cycle: no node has zero parents but we have nodes left.
			// Log and dump the remainder into a single best-effort
			// group so the system still boots.
			var stuck []string
			for name := range remaining {
				stuck = append(stuck, name)
			}
			fmt.Fprintf(os.Stderr,
				"lohar: dependency cycle among %v; starting remainder in arbitrary order\n",
				stuck)
			var lastGroup []*Unit
			for name := range remaining {
				lastGroup = append(lastGroup, byName[name])
			}
			groups = append(groups, lastGroup)
			break
		}
		groups = append(groups, ready)
		for _, u := range ready {
			delete(remaining, u.Canonical)
			for child := range remaining {
				delete(parents[child], u.Canonical)
			}
		}
	}
	return groups
}

// parseDepList splits a directive value like
//
//	After=foo.service bar.service baz.target
//
// into canonical names without suffix: ["foo", "bar", "baz"]. systemd's
// directive format permits multiple deps separated by whitespace on one
// line, plus repeated directive lines (handled at the caller via
// getAll). Targets and other unit types are returned with their suffix
// stripped because our Registry indexes by canonical-base name.
//
// Leading/trailing whitespace is trimmed; empty tokens are skipped.
func parseDepList(raw string) []string {
	fields := strings.Fields(raw)
	out := make([]string, 0, len(fields))
	for _, f := range fields {
		base, _ := splitSuffix(f)
		if base != "" {
			out = append(out, base)
		}
	}
	return out
}
