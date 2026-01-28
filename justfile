# Alwaysrunning - Justfile
# Tiny, opinionated process supervisor

# ============================================
# BUILD COMMANDS
# ============================================

# Build debug binary
build:
    cargo build

# Build release binary
build-release:
    cargo build --release

# ============================================
# TEST COMMANDS
# ============================================

# Run all tests
test:
    cargo test

# ============================================
# CODE QUALITY
# ============================================

# Check formatting
fmt-check:
    cargo fmt --all -- --check

# Format code
fmt:
    cargo fmt --all

# Run clippy lints
clippy:
    cargo clippy --all-targets -- -D warnings

# Run all checks
check: fmt-check clippy test
    @echo "All checks passed!"

# ============================================
# CLEANUP COMMANDS
# ============================================

# Clean build artifacts
clean:
    cargo clean

# ============================================
# RELEASE COMMANDS
# ============================================

# Show current version
version:
    @grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'

# Release using cargo-release (recommended)
# Usage: just release-cargo [patch|minor|major]
release-cargo TYPE="patch":
    #!/usr/bin/env bash
    set -e
    cargo release {{TYPE}} --no-publish --no-verify --no-confirm --execute

# Dry-run release to see what would happen
release-dry-run:
    cargo release patch --no-publish --no-verify --no-confirm

# ============================================
# HELP
# ============================================

# Show all available commands
help:
    @echo "Alwaysrunning - Available Commands"
    @echo ""
    @echo "Build:"
    @echo "  build         - Build debug binary"
    @echo "  build-release - Build release binary"
    @echo ""
    @echo "Testing:"
    @echo "  test - Run all tests"
    @echo ""
    @echo "Code Quality:"
    @echo "  fmt       - Format code"
    @echo "  fmt-check - Check formatting"
    @echo "  clippy    - Run clippy lints"
    @echo "  check     - Run all checks"
    @echo ""
    @echo "Cleanup:"
    @echo "  clean - Clean build artifacts"
    @echo ""
    @echo "Release:"
    @echo "  version        - Show current version"
    @echo "  release-cargo  - Release using cargo-release (recommended)"
    @echo "  release-dry-run - Dry-run cargo-release"
    @echo ""
    @echo "  help  - Show this help"
