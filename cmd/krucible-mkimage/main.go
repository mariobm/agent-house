// Command krucible-mkimage builds a krucible block-root base image from an OCI
// reference (e.g. "alpine", "ubuntu:24.04") via oci.PullAndConvert, injecting
// the forge agent at /init.krun. It's a dev/bench helper: in production the
// server pulls and converts images itself. Pure Go (no cgo) — needs `mke2fs`
// (e2fsprogs) and network access to the registry.
//
// Usage: krucible-mkimage <oci-ref> <out.img> [forge-path]
//
//	forge-path defaults to ./forge; it must be a linux/<host-arch> binary
//	(guest arch == host arch under HVF/KVM).
package main

import (
	"context"
	"fmt"
	"os"
	"runtime"
	"time"

	"github.com/mariobm/agent-house/pkg/oci"
)

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: krucible-mkimage <oci-ref> <out.img> [forge-path]")
		os.Exit(2)
	}
	ref, out := os.Args[1], os.Args[2]
	forge := "./forge"
	if len(os.Args) > 3 {
		forge = os.Args[3]
	}
	if _, err := os.Stat(forge); err != nil {
		fmt.Fprintf(os.Stderr, "krucible-mkimage: forge binary not found at %q (cross-build with GOOS=linux GOARCH=%s): %v\n", forge, runtime.GOARCH, err)
		os.Exit(1)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()
	cfg, err := oci.PullAndConvert(ctx, ref, out, forge,
		oci.WithPlatform("linux", runtime.GOARCH),
		oci.WithProgress(func(s string) { fmt.Fprintln(os.Stderr, "  "+s) }),
	)
	if err != nil {
		fmt.Fprintf(os.Stderr, "krucible-mkimage: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("built %s from %s (linux/%s)\n", out, ref, runtime.GOARCH)
	if cfg != nil {
		fmt.Printf("  cmd=%v workdir=%q user=%q size=%dMB\n", cfg.Cmd, cfg.WorkingDir, cfg.User, cfg.TotalSize>>20)
	}
}
