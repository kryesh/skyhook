# Use the installed Rust toolchain, Zig, just, and mdBook; no bootstrap or version pins.
set shell := ["bash", "-euo", "pipefail", "-c"]

# List common development and documentation commands.
default:
    @just --list

# Debug CLI with embedded SSH shims (requires Zig and the Rust shim targets).
build:
    cargo build --locked -p skyhook-agent

# Local-only debug CLI, without embedded shims or Zig.
build-local:
    cargo build --locked -p skyhook-agent --no-default-features --features tui

# Release CLI with embedded SSH shims; build.rs owns the target matrix.
release:
    cargo build --locked --release -p skyhook-agent

# Build the native Linux SSH shim only, without cross-compilation.
shim:
    cargo build --locked -p skyhook-agent --bin linux-ssh --no-default-features --features shim-bin

# Run core and CLI tests without rebuilding the embedded shim bundle.
test:
    cargo test --locked -p skyhook-agent-core --all-targets
    cargo test --locked -p skyhook-agent --all-targets --no-default-features --features tui

# Lint the workspace without requiring Zig or cross-compilation targets.
lint:
    cargo clippy --locked --workspace --all-targets --no-default-features --features tui -- -D warnings

# Format all workspace Rust code.
fmt:
    cargo fmt --all

# Check formatting without modifying files.
fmt-check:
    cargo fmt --all -- --check

# Build docs/src into docs/book using the currently installed mdbook.
docs:
    mdbook build docs

# Check rendered local links and anchors, plus repository README links.
docs-check: docs
    python3 scripts/check-docs.py

# Serve the book locally; mdBook watches for source changes.
docs-serve:
    mdbook serve docs

# Prepare the static GitHub Pages artifact (no Jekyll processing).
pages-build: docs
    touch docs/book/.nojekyll

# Build, then publish committed sources via a temporary worktree; never force-push.
pages-publish remote='origin' branch='pages':
    bash scripts/pages-publish.sh {{ quote(remote) }} {{ quote(branch) }}
