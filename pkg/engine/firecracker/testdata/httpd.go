//go:build ignore

// Tiny HTTP server for integration tests. Compiled statically and
// injected into VMs via FileWrite. Serves files from /tmp.
package main

import (
	"net/http"
	"os"
)

func main() {
	dir := "/tmp"
	if len(os.Args) > 1 {
		dir = os.Args[1]
	}
	http.ListenAndServe(":8888", http.FileServer(http.Dir(dir)))
}
