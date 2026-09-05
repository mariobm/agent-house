.PHONY: build vmm krucible test clean release

VERSION ?= $(shell git describe --tags --always --dirty)

# libkrucible (our libkrun fork). KRUCIBLE_PREFIX is the link prefix `make vmm`
# points at; if unbuilt, vmm falls back to the system (Homebrew) libkrun.
LIBKRUCIBLE ?= libkrucible
KRUCIBLE_PREFIX ?= $(abspath $(LIBKRUCIBLE)/_install)

# Build the ahvm binary with version injection
build:
	go build -ldflags="-s -w -X main.version=$(VERSION)" -o ahvm ./cmd/ahvm/

# Build lohar (guest agent) for Linux
lohar:
	GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -ldflags="-s -w" -o lohar ./cmd/lohar/

# Build the per-VM libkrun helper (krucible engine). cgo + libkrun via
# pkg-config; on macOS it must be codesigned with the hypervisor entitlement
# to use HVF. The daemon spawns this binary per sandbox.
# Build libkrucible (our libkrun fork) + assemble the link prefix for `make vmm`.
krucible:
	scripts/krucible-build-lib.sh "$(LIBKRUCIBLE)" "$(KRUCIBLE_PREFIX)"

vmm:
	PKG_CONFIG_PATH="$(KRUCIBLE_PREFIX)/lib/pkgconfig:$$PKG_CONFIG_PATH" \
		CGO_ENABLED=1 go build -tags krucible -ldflags="-X main.version=$(VERSION)" -o ahvm-vmm ./cmd/vmm/
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		codesign --force --entitlements cmd/vmm/hvf-entitlements.plist -s - ahvm-vmm && \
		echo "codesigned ahvm-vmm for HVF"; \
	fi
	@echo "Built ahvm-vmm (links libkrucible if built, else system libkrun)"

# Build the per-owner network gateway (krucible net backend). Pure Go (gVisor);
# the daemon spawns it per owner when krucible_net_backend is set. Runs on the
# host, so build it for the host platform like `ahvm`.
netd:
	go build -ldflags="-s -w" -o ahvm-netd ./cmd/ahvm-netd/
	@echo "Built ahvm-netd"

test:
	go test ./... -count=1 -timeout 120s

# Cross-compile for all platforms
release:
	@mkdir -p dist
	GOOS=darwin GOARCH=arm64 go build -ldflags="-s -w -X main.version=$(VERSION)" \
		-o dist/ahvm-darwin-arm64 ./cmd/ahvm/
	GOOS=darwin GOARCH=amd64 go build -ldflags="-s -w -X main.version=$(VERSION)" \
		-o dist/ahvm-darwin-amd64 ./cmd/ahvm/
	GOOS=linux GOARCH=amd64 go build -ldflags="-s -w -X main.version=$(VERSION)" \
		-o dist/ahvm-linux-amd64 ./cmd/ahvm/
	GOOS=linux GOARCH=arm64 go build -ldflags="-s -w -X main.version=$(VERSION)" \
		-o dist/ahvm-linux-arm64 ./cmd/ahvm/
	@echo "Built $(VERSION) for 4 platforms in dist/"

clean:
	rm -f ahvm lohar ahvm-vmm ahvm-netd
	rm -rf dist/
