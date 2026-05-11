.PHONY: help all server sidecar check test integration clippy clean clean-all

# Directories
SIDECAR_BUILD_DIR := slang-sidecar/build
RUST_TARGET_DIR := target
RELEASE_BIN := $(RUST_TARGET_DIR)/release/mimir-server
SIDECAR_BIN := $(SIDECAR_BUILD_DIR)/mimir-slang-sidecar

# Default target
help:
	@echo "Mimir build targets:"
	@echo ""
	@echo "  make all       - build server and sidecar (default)"
	@echo "  make server    - build Rust server (release)"
	@echo "  make sidecar   - build C++ sidecar"
	@echo "  make check     - cargo check"
	@echo "  make test      - run all cargo unit tests"
	@echo "  make integration - run python LSP integration tests (needs release server)"
	@echo "  make clippy    - run cargo clippy linter"
	@echo "  make clean     - remove build artifacts (keep dependencies)"
	@echo "  make clean-all - full clean (removes downloaded dependencies)"
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

# Full clean including downloaded dependencies
clean-all: clean
	rm -rf $(SIDECAR_BUILD_DIR)
