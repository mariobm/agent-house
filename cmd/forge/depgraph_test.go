//go:build linux

package main

import (
	"sort"
	"strings"
	"testing"
)

// makeUnit returns a synthetic Unit with the given canonical name and
// optional After= / Before= directives. Tests use this to build a
// graph without round-tripping through the filesystem; buildStartGraph
// only reads u.Canonical and u.Sections, never u.Path.
func makeUnit(canonical string, after, before []string) *Unit {
	sections := map[string][]kvPair{}
	if len(after) > 0 {
		sections["Unit"] = append(sections["Unit"], kvPair{
			key: "After", value: strings.Join(after, " "),
		})
	}
	if len(before) > 0 {
		sections["Unit"] = append(sections["Unit"], kvPair{
			key: "Before", value: strings.Join(before, " "),
		})
	}
	return &Unit{
		Canonical: canonical,
		Suffix:    ".service",
		Sections:  serviceFile{sections: sections},
	}
}

// names extracts canonical names from a slice of units, sorted, so test
// assertions don't depend on map-iteration order within a parallel group.
func names(units []*Unit) []string {
	out := make([]string, len(units))
	for i, u := range units {
		out[i] = u.Canonical
	}
	sort.Strings(out)
	return out
}

func TestBuildStartGraphEmpty(t *testing.T) {
	if got := buildStartGraph(nil); got != nil {
		t.Errorf("empty input should return nil, got %v", got)
	}
	if got := buildStartGraph([]*Unit{}); got != nil {
		t.Errorf("empty slice should return nil, got %v", got)
	}
}

func TestBuildStartGraphSingleNode(t *testing.T) {
	u := makeUnit("solo", nil, nil)
	groups := buildStartGraph([]*Unit{u})
	if len(groups) != 1 || len(groups[0]) != 1 || groups[0][0] != u {
		t.Errorf("one node, no deps: got %v, want [[solo]]", groups)
	}
}

func TestBuildStartGraphLinear(t *testing.T) {
	// A After=B, B After=C  =>  start order C, then B, then A.
	a := makeUnit("a", []string{"b.service"}, nil)
	b := makeUnit("b", []string{"c.service"}, nil)
	c := makeUnit("c", nil, nil)

	groups := buildStartGraph([]*Unit{a, b, c})
	want := [][]string{{"c"}, {"b"}, {"a"}}
	got := make([][]string, len(groups))
	for i, g := range groups {
		got[i] = names(g)
	}
	if !equalSliceOfSlices(got, want) {
		t.Errorf("linear: got %v, want %v", got, want)
	}
}

func TestBuildStartGraphParallel(t *testing.T) {
	// A After=C, B After=C, C standalone  =>  [C], [A,B] (A,B parallel)
	a := makeUnit("a", []string{"c.service"}, nil)
	b := makeUnit("b", []string{"c.service"}, nil)
	c := makeUnit("c", nil, nil)

	groups := buildStartGraph([]*Unit{a, b, c})
	if len(groups) != 2 {
		t.Fatalf("got %d groups, want 2: %v", len(groups), groups)
	}
	if got := names(groups[0]); !equalSlice(got, []string{"c"}) {
		t.Errorf("group 0 = %v, want [c]", got)
	}
	if got := names(groups[1]); !equalSlice(got, []string{"a", "b"}) {
		t.Errorf("group 1 = %v, want [a b]", got)
	}
}

func TestBuildStartGraphBeforeIsInverted(t *testing.T) {
	// A Before=B is equivalent to B After=A.
	// Expected order: A first (no parents), then B (parent: A).
	a := makeUnit("a", nil, []string{"b.service"})
	b := makeUnit("b", nil, nil)

	groups := buildStartGraph([]*Unit{a, b})
	want := [][]string{{"a"}, {"b"}}
	got := make([][]string, len(groups))
	for i, g := range groups {
		got[i] = names(g)
	}
	if !equalSliceOfSlices(got, want) {
		t.Errorf("Before= inversion: got %v, want %v", got, want)
	}
}

func TestBuildStartGraphMissingDepIsIgnored(t *testing.T) {
	// A After=ghost.service where ghost is not in the input set.
	// The edge is dropped (we can't gate on something we don't know
	// about); A starts in the first group.
	a := makeUnit("a", []string{"ghost.service"}, nil)
	groups := buildStartGraph([]*Unit{a})
	if len(groups) != 1 || groups[0][0].Canonical != "a" {
		t.Errorf("missing dep: got %v, want [[a]]", groups)
	}
}

func TestBuildStartGraphSelfEdgeIgnored(t *testing.T) {
	// A After=A.service. Real unit-file bug; we don't crash, just drop
	// the self-edge so A starts normally.
	a := makeUnit("a", []string{"a.service"}, nil)
	groups := buildStartGraph([]*Unit{a})
	if len(groups) != 1 || groups[0][0].Canonical != "a" {
		t.Errorf("self-edge: got %v, want [[a]]", groups)
	}
}

func TestBuildStartGraphCycle(t *testing.T) {
	// A After=B, B After=A. systemd logs a warning and breaks the cycle
	// by ordering arbitrarily. We do the same: dump the unsorted set
	// into a final best-effort group and continue.
	a := makeUnit("a", []string{"b.service"}, nil)
	b := makeUnit("b", []string{"a.service"}, nil)

	groups := buildStartGraph([]*Unit{a, b})
	// Either:
	//   - Single group containing both (the cycle-break path)
	//   - Should NOT be more than one group (no ordering established)
	if len(groups) != 1 {
		t.Errorf("cycle should produce 1 best-effort group, got %d: %v", len(groups), groups)
	}
	if got := names(groups[0]); !equalSlice(got, []string{"a", "b"}) {
		t.Errorf("cycle group = %v, want [a b]", got)
	}
}

func TestBuildStartGraphPartialCycle(t *testing.T) {
	// C standalone, A <-> B cycle. C should still start in its own
	// group; the cycle pair becomes a best-effort tail.
	a := makeUnit("a", []string{"b.service"}, nil)
	b := makeUnit("b", []string{"a.service"}, nil)
	c := makeUnit("c", nil, nil)

	groups := buildStartGraph([]*Unit{a, b, c})
	if len(groups) != 2 {
		t.Fatalf("got %d groups, want 2: %v", len(groups), groups)
	}
	if got := names(groups[0]); !equalSlice(got, []string{"c"}) {
		t.Errorf("group 0 = %v, want [c]", got)
	}
	if got := names(groups[1]); !equalSlice(got, []string{"a", "b"}) {
		t.Errorf("group 1 (cycle break) = %v, want [a b]", got)
	}
}

func TestBuildStartGraphMixedSuffixes(t *testing.T) {
	// Real units mix .service / .target / .socket dependencies. We
	// strip suffixes when matching against canonical names.
	a := makeUnit("a", []string{"network.target", "b.service"}, nil)
	b := makeUnit("b", nil, nil)

	groups := buildStartGraph([]*Unit{a, b})
	// network.target isn't in the input set so its edge is dropped;
	// only b->a remains.
	want := [][]string{{"b"}, {"a"}}
	got := make([][]string, len(groups))
	for i, g := range groups {
		got[i] = names(g)
	}
	if !equalSliceOfSlices(got, want) {
		t.Errorf("mixed suffixes: got %v, want %v", got, want)
	}
}

func TestBuildStartGraphMultipleAfterLines(t *testing.T) {
	// systemd permits multiple After= lines; getAll returns each one
	// separately. A unit with two After= lines should accumulate both
	// dependencies.
	a := &Unit{
		Canonical: "a",
		Suffix:    ".service",
		Sections: serviceFile{sections: map[string][]kvPair{
			"Unit": {
				{key: "After", value: "b.service"},
				{key: "After", value: "c.service"},
			},
		}},
	}
	b := makeUnit("b", nil, nil)
	c := makeUnit("c", nil, nil)

	groups := buildStartGraph([]*Unit{a, b, c})
	if len(groups) != 2 {
		t.Fatalf("got %d groups, want 2: %v", len(groups), groups)
	}
	if got := names(groups[0]); !equalSlice(got, []string{"b", "c"}) {
		t.Errorf("group 0 = %v, want [b c]", got)
	}
	if got := names(groups[1]); !equalSlice(got, []string{"a"}) {
		t.Errorf("group 1 = %v, want [a]", got)
	}
}

func TestBuildStartGraphAfterListSyntax(t *testing.T) {
	// systemd accepts whitespace-separated deps on one line:
	//   After=foo.service bar.service baz.service
	// parseDepList handles this; verify end-to-end.
	a := makeUnit("a", []string{"b.service c.service"}, nil)
	b := makeUnit("b", nil, nil)
	c := makeUnit("c", nil, nil)

	groups := buildStartGraph([]*Unit{a, b, c})
	if len(groups) != 2 {
		t.Fatalf("got %d groups, want 2: %v", len(groups), groups)
	}
	if got := names(groups[0]); !equalSlice(got, []string{"b", "c"}) {
		t.Errorf("group 0 = %v, want [b c]", got)
	}
}

func TestParseDepList(t *testing.T) {
	cases := []struct {
		in   string
		want []string
	}{
		{"", nil},
		{"foo.service", []string{"foo"}},
		{"foo.service bar.service", []string{"foo", "bar"}},
		{"  foo.service   bar.service  ", []string{"foo", "bar"}}, // whitespace tolerant
		{"foo.target", []string{"foo"}},                            // suffix stripped regardless of type
		{"foo.socket bar.timer", []string{"foo", "bar"}},
		{"foo bar", []string{"foo", "bar"}}, // bare names also accepted (default suffix .service)
	}
	for _, c := range cases {
		got := parseDepList(c.in)
		if !equalSlice(got, c.want) {
			t.Errorf("parseDepList(%q) = %v, want %v", c.in, got, c.want)
		}
	}
}

// --- Tiny slice helpers (avoiding a reflect.DeepEqual dependency) ---

func equalSlice(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func equalSliceOfSlices(a, b [][]string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if !equalSlice(a[i], b[i]) {
			return false
		}
	}
	return true
}
