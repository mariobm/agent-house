.PHONY: build test check vmm forge netd bundle

# Portable tools and unit tests do not need the native VMM or a hypervisor.
build:
	cargo build --manifest-path rust/Cargo.toml --locked -p ahvm-cli -p ahvm-daemon -p ahvm-netd

test:
	cargo test --manifest-path rust/Cargo.toml --locked --workspace --exclude ahvm-vmm
	cargo build --manifest-path rust/Cargo.toml --locked -p ahvm-cli
	python3 scripts/test-rust-cli.py rust/target/debug/ahvm

check:
	cargo fmt --manifest-path rust/Cargo.toml --all --check
	cargo clippy --manifest-path rust/Cargo.toml --locked --workspace --exclude ahvm-vmm --all-targets -- -D warnings

vmm:
	cargo build --manifest-path rust/Cargo.toml --locked -p ahvm-vmm

forge:
	cargo build --manifest-path rust/Cargo.toml --locked -p ahvm-forge --target x86_64-unknown-linux-musl

netd:
	cargo build --manifest-path rust/Cargo.toml --locked -p ahvm-netd

# Native Linux x86_64; OUT must not already exist. Never publishes a release.
bundle:
	scripts/package-rust.sh "$(OUT)"
