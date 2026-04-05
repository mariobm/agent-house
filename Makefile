.PHONY: build test clean release

VERSION ?= $(shell git describe --tags --always --dirty)

# Build the bhatti binary with version injection
build:
	go build -ldflags="-s -w -X main.version=$(VERSION)" -o bhatti ./cmd/bhatti/

# Build lohar (guest agent) for Linux
lohar:
	GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -ldflags="-s -w" -o lohar ./cmd/lohar/

test:
	go test ./... -count=1 -timeout 120s

# Cross-compile for all platforms
release:
	@mkdir -p dist
	GOOS=darwin GOARCH=arm64 go build -ldflags="-s -w -X main.version=$(VERSION)" \
		-o dist/bhatti-darwin-arm64 ./cmd/bhatti/
	GOOS=darwin GOARCH=amd64 go build -ldflags="-s -w -X main.version=$(VERSION)" \
		-o dist/bhatti-darwin-amd64 ./cmd/bhatti/
	GOOS=linux GOARCH=amd64 go build -ldflags="-s -w -X main.version=$(VERSION)" \
		-o dist/bhatti-linux-amd64 ./cmd/bhatti/
	GOOS=linux GOARCH=arm64 go build -ldflags="-s -w -X main.version=$(VERSION)" \
		-o dist/bhatti-linux-arm64 ./cmd/bhatti/
	@echo "Built $(VERSION) for 4 platforms in dist/"

clean:
	rm -f bhatti lohar
	rm -rf dist/
