# microdns — build / cross-compile

TARGET_MUSL ?= aarch64-unknown-linux-musl
CARGO ?= cargo
RUSTUP_TOOLCHAIN ?= stable
export RUSTUP_TOOLCHAIN

.PHONY: all build release release-musl check test test-release-assertions \
	clean fmt clippy hub-upload deploy deps-update

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

# Refresh git crates (dcc-daemon) and rewrite Cargo.lock. Commit the lockfile afterwards.
deps-update:
	$(CARGO) update -p dcc-daemon

clean:
	$(CARGO) clean
	rm -rf dist

# --- Hub deploy (RO rootfs: binary lives on /data) -------------------------
# Hub runs Dropbear. Older images lack /usr/libexec/sftp-server; -O uses legacy scp.
# Harmless on images that ship openssh sftp-server (bigfred-os defconfig).
HUB ?= 192.168.0.1
HUB_USER ?= root
HUB_SSH ?= $(HUB_USER)@$(HUB)
SCP ?= scp
SCP_OPTS ?= -O
SSH ?= ssh
DIST_ARM64 ?= dist/microdns-linux-arm64
HUB_BIN_DIR ?= /data/opt/microdns

# Build arm64 musl binary and upload to the hub's writable /data partition.
# Requires /etc/init.d/microdns to prefer $(HUB_BIN_DIR)/microdns (bigfred-os overlay).
deploy: hub-upload

hub-upload: release-musl
	@test -f $(DIST_ARM64) || { echo "error: $(DIST_ARM64) missing — run make release-musl" >&2; exit 1; }
	$(SCP) $(SCP_OPTS) $(DIST_ARM64) $(HUB_SSH):/tmp/microdns
	$(SSH) $(HUB_SSH) 'mkdir -p $(HUB_BIN_DIR) && cp /tmp/microdns $(HUB_BIN_DIR)/microdns && chmod 755 $(HUB_BIN_DIR)/microdns && rm -f /tmp/microdns && microinit stop microdns; microinit start microdns'
