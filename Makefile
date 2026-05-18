.PHONY: help all server sidecar verible check test integration clippy clean clean-all

# Directories
SIDECAR_BUILD_DIR := slang-sidecar/build
RUST_TARGET_DIR   := target
RELEASE_BIN       := $(RUST_TARGET_DIR)/release/mimir-server
SIDECAR_BIN       := $(SIDECAR_BUILD_DIR)/mimir-slang-sidecar

# Verible formatter (for running integration tests; not required for the server itself)
# Override VERIBLE_VERSION to pin a different release.
VERIBLE_VERSION  ?= v0.0-4053-g89d4d98a
VERIBLE_PLATFORM ?= linux-static-x86_64
VERIBLE_DIR      := tools/verible
VERIBLE_BIN      := $(VERIBLE_DIR)/bin/verible-verilog-format
VERIBLE_URL      := https://github.com/chipsalliance/verible/releases/download/$(VERIBLE_VERSION)/verible-$(VERIBLE_VERSION)-$(VERIBLE_PLATFORM).tar.gz

# Default target
help:
	@echo "Mimir build targets:"
	@echo ""
	@echo "  make all         - build server and sidecar"
	@echo "  make server      - build Rust server (release)"
	@echo "  make sidecar     - build C++ sidecar"
	@echo "  make verible     - download verible-verilog-format for integration tests"
	@echo "  make check       - cargo check"
	@echo "  make test        - run all cargo unit tests"
	@echo "  make integration - run python LSP integration tests (needs release server)"
	@echo "  make clippy      - run cargo clippy linter"
	@echo "  make clean       - remove build artifacts (keep dependencies)"
	@echo "  make clean-all   - full clean (removes downloaded dependencies)"
	@echo ""
	@echo "To run formatter integration tests:"
	@echo "  make verible"
	@echo "  VERIBLE_BIN=$(CURDIR)/$(VERIBLE_BIN) cargo test -p mimir-server -- --include-ignored format"
	@echo ""

# Build everything
all: server sidecar

# Build Rust server in release mode
server: $(RELEASE_BIN)

$(RELEASE_BIN):
	cargo build --release --workspace

# Build C++ sidecar
sidecar: $(SIDECAR_BIN)

$(SIDECAR_BIN):
	cmake -G Ninja -S slang-sidecar -B $(SIDECAR_BUILD_DIR) -DCMAKE_BUILD_TYPE=Release
	cmake --build $(SIDECAR_BUILD_DIR)

# Download verible-verilog-format for local development and integration tests.
# The binary is placed in tools/verible/bin/ which is listed in .gitignore.
# VERIBLE_VERSION and VERIBLE_PLATFORM can be overridden on the command line:
#   make verible VERIBLE_VERSION=v0.0-XXXX VERIBLE_PLATFORM=darwin-arm64
$(VERIBLE_BIN):
	mkdir -p $(VERIBLE_DIR)
	curl -fsSL "$(VERIBLE_URL)" | tar xz -C $(VERIBLE_DIR) --strip-components=1
	@echo "verible-verilog-format installed at $(VERIBLE_BIN)"

verible: $(VERIBLE_BIN)

# Cargo check
check:
	cargo check --workspace

# Run tests
test:
	cargo test --workspace

# Python LSP integration tests — drive the real server over stdio against
# the bundled UVM example. Requires the release binary; we build it
# automatically so the integration suite is always exercising the
# just-compiled server.
integration: $(RELEASE_BIN)
	python3 -m unittest discover -v -s tests -t . -p 'test_*.py'

# Lint with clippy
clippy:
	cargo clippy --workspace -- -D warnings

# Clean build artifacts
clean:
	cargo clean
	rm -rf $(SIDECAR_BUILD_DIR)/CMakeFiles $(SIDECAR_BUILD_DIR)/*.cmake $(SIDECAR_BUILD_DIR)/build.log

# Full clean including downloaded dependencies and downloaded tools
clean-all: clean
	rm -rf $(SIDECAR_BUILD_DIR) $(VERIBLE_DIR)
