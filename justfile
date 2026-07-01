# minibucket — task runner
#
# Install `just`:  winget install Casey.Just   (or  cargo install just)
# List recipes:    just            (or  just --list)
#
# minibucket is a single-crate, dependency-free Rust project. The release
# recipes below stamp the version straight into Cargo.toml / Cargo.lock with
# plain regex (no Node, no cargo-edit) and drive the GitHub release workflow,
# which builds the multi-arch binaries + the ghcr.io Docker image.

# Run recipes through PowerShell so the multi-line release bodies work on Windows.
set windows-shell := ["pwsh.exe", "-NoLogo", "-NoProfile", "-Command"]

# Default: show the recipe list.
default:
    @just --list

# ---------------------------------------------------------------------------
# Dev
# ---------------------------------------------------------------------------

# Run the server from source, passing through any args:  just run --anonymous
run *ARGS:
    cargo run -- {{ARGS}}

# Build a debug binary.
build:
    cargo build

# Build the optimized release binary (-> target/release/minibucket).
build-release:
    cargo build --release

# ---------------------------------------------------------------------------
# Quality
# ---------------------------------------------------------------------------

# Cargo type-check (faster than a full build).
check:
    cargo check

# Lint with clippy (warnings as errors).
clippy:
    cargo clippy --all-targets -- -D warnings

# Format all Rust code.
fmt:
    cargo fmt --all

# Verify formatting without writing changes.
fmt-check:
    cargo fmt --all -- --check

# Run the Rust test suite.
test:
    cargo test

# Run the Python smoke test against an already-running server on :9123.
# Start one first, e.g.:
#   just run --bind 127.0.0.1:9123 --access-key alice --secret-key alicepass
smoke:
    python smoketest.py

# Everything CI checks: formatting, clippy, tests.
ci: fmt-check clippy test

# ---------------------------------------------------------------------------
# Docker (mirrors what the release workflow publishes to ghcr.io)
# ---------------------------------------------------------------------------

# Build the scratch image locally.
docker-build:
    docker build -t minibucket:dev .

# Run the local image (data in a named volume, exposed on :9000).
docker-run:
    docker run --rm -p 9000:9000 -v minibucket-data:/data minibucket:dev

# ---------------------------------------------------------------------------
# Release
# ---------------------------------------------------------------------------

# Print the current version (from Cargo.toml).
version:
    @(Select-String -Path Cargo.toml -Pattern '^version = "(.*)"').Matches[0].Groups[1].Value

# Stamp a version into Cargo.toml + Cargo.lock. Accepts a bump keyword or an
# explicit version. Examples:
#   just set-version patch        just set-version minor        just set-version 0.2.0
set-version BUMP="patch":
    node scripts/set-version.mjs {{BUMP}}

# Cut a release: bump the version, commit, tag `v<x.y.z>`, and push -> triggers
# the release workflow (multi-arch binaries + ghcr.io Docker image). Examples:
#   just release            just release minor            just release 1.0.0
release BUMP="patch":
    node scripts/release.mjs {{BUMP}}

# ---------------------------------------------------------------------------
# Housekeeping
# ---------------------------------------------------------------------------

# Remove build artifacts.
clean:
    cargo clean
