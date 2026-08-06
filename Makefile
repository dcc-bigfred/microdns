# microdns — build / cross-compile / OCI helpers

TARGET_MUSL ?= aarch64-unknown-linux-musl
CARGO ?= cargo
RUSTUP_TOOLCHAIN ?= stable
export RUSTUP_TOOLCHAIN

CI_SCRIPTS_REPO ?= https://github.com/dcc-bigfred/.github.git
CI_SCRIPTS_REF  ?= v1
CI_SCRIPTS_DIR  ?= .ci-github

OCI_IMAGE  ?= ghcr.io/dcc-bigfred/microdns-linux-arm64
OCI_TITLE  ?= microdns
OCI_LAYERS ?= microdns-linux-arm64=application/vnd.dcc-bigfred.microdns.linux.arm64.v1
OCI_ELF_LAYERS  ?= microdns-linux-arm64
OCI_ELF_SECTION ?= .microdns.version

.PHONY: all build release release-musl check test test-release-assertions \
	clean fmt clippy ci-scripts-update publish-oci retag-oci

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

$(CI_SCRIPTS_DIR)/.ok:
	@echo "Cloning $(CI_SCRIPTS_REPO) @ $(CI_SCRIPTS_REF) → $(CI_SCRIPTS_DIR)"
	@rm -rf "$(CI_SCRIPTS_DIR)"
	@git clone --depth 1 --branch "$(CI_SCRIPTS_REF)" "$(CI_SCRIPTS_REPO)" "$(CI_SCRIPTS_DIR)" \
		|| { echo "error: failed to clone $(CI_SCRIPTS_REPO) @ $(CI_SCRIPTS_REF)"; exit 1; }
	@touch "$@"

ci-scripts-update:
	rm -rf "$(CI_SCRIPTS_DIR)"
	$(MAKE) "$(CI_SCRIPTS_DIR)/.ok"

publish-oci: release-musl $(CI_SCRIPTS_DIR)/.ok
	cd dist && \
	OCI_IMAGE="$(OCI_IMAGE)" OCI_TITLE="$(OCI_TITLE)" OCI_LAYERS="$(OCI_LAYERS)" \
		"../$(CI_SCRIPTS_DIR)/scripts/publish-oci-linux.sh"

retag-oci: $(CI_SCRIPTS_DIR)/.ok
	@test -n "$(TAG)" || { echo "usage: make retag-oci TAG=v0.1.0"; exit 1; }
	OCI_IMAGE="$(OCI_IMAGE)" OCI_TITLE="$(OCI_TITLE)" OCI_LAYERS="$(OCI_LAYERS)" \
	OCI_ELF_LAYERS="$(OCI_ELF_LAYERS)" OCI_ELF_SECTION="$(OCI_ELF_SECTION)" \
		"$(CI_SCRIPTS_DIR)/scripts/retag-oci-linux.sh" "$(TAG)"
