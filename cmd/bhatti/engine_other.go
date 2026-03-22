//go:build !linux

package main

import (
	"fmt"

	"github.com/sahil-shubham/bhatti/pkg"
	"github.com/sahil-shubham/bhatti/pkg/engine"
)

func newFirecrackerEngine(cfg *pkg.Config) (engine.Engine, error) {
	return nil, fmt.Errorf("bhatti requires Linux with KVM (firecracker engine only)")
}
