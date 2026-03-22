.PHONY: build test clean

# Build the bhatti binary
build:
	go build -o bhatti ./cmd/bhatti/

test:
	go test ./... -count=1 -timeout 120s

clean:
	rm -f bhatti
