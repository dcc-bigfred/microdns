# microdns — build / cross-compile

TARGET_MUSL ?= aarch64-unknown-linux-musl
CARGO ?= cargo
RUSTUP_TOOLCHAIN ?= stable
export RUSTUP_TOOLCHAIN

.PHONY: all build release release-musl check test test-release-assertions \
	clean fmt clippy

all: build

build:
	$(CARGO) build

release:
	$(CARGO) build --release

release-musl:
	RUSTFLAGS='-C target-feature=+crt-static' \
		$(CARGO) build --release --target $(TARGET_MUSL)
	@mkdir -p dist
	cp -f target/$(TARGET_MUSL)/release/microdns dist/microdns-linux-arm64
	@echo "wrote dist/microdns-linux-arm64"

check:
	$(CARGO) check

test:
	$(CARGO) test

test-release-assertions:
	$(CARGO) test --profile release-assertions

fmt:
	$(CARGO) fmt

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

clean:
	$(CARGO) clean
	rm -rf dist
