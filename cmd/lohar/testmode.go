//go:build linux

package main

import (
	"fmt"
	"net"
	"os"
)

func runTestMode() {
	// Ensure PATH is set (matches PID 1 init behavior).
	if os.Getenv("PATH") == "" {
		os.Setenv("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
	}
	controlSock := os.Getenv("LOHAR_SOCK")
	forwardSock := os.Getenv("LOHAR_FWD_SOCK")
	if controlSock == "" || forwardSock == "" {
		fmt.Fprintln(os.Stderr, "LOHAR_SOCK and LOHAR_FWD_SOCK required")
		os.Exit(1)
	}

	os.Remove(controlSock)
	os.Remove(forwardSock)

	lnControl, err := net.Listen("unix", controlSock)
	if err != nil {
		fmt.Fprintf(os.Stderr, "listen control: %v\n", err)
		os.Exit(1)
	}
	lnForward, err := net.Listen("unix", forwardSock)
	if err != nil {
		fmt.Fprintf(os.Stderr, "listen forward: %v\n", err)
		os.Exit(1)
	}

	fmt.Fprintln(os.Stderr, "lohar: test mode ready")
	go acceptLoop(lnControl, handleControlConnection)
	go acceptLoop(lnForward, handleForwardConnection)

	select {} // block forever
}
