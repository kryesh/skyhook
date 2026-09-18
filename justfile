# Use the installed Rust toolchain, Zig, and just; `just setup` installs the cargo tools.
set shell := ["bash", "-euo", "pipefail", "-c"]

# List common development and documentation commands.
default:
    @just --list

# linux-ssh shim targets; each is staged as target/shims/linux-ssh-<arch>.
linux_ssh_targets := "x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

# Install Rust-side tooling and shim targets (Zig must be installed separately).
setup:
    rustup component add clippy rustfmt
    rustup target add {{ linux_ssh_targets }}
    cargo install --locked cargo-zigbuild cargo-nextest mdbook

# Debug CLI; reads whatever is in target/shims live from disk.
run:
    cargo run --locked --features tui

# Debug CLI; reads whatever is in target/shims live from disk.
build:
    cargo build --locked --features tui

# Release CLI; embeds whatever is in target/shims at compile time.
build-release:
    # rust-embed cannot see newly added files; force the embedding module to rebuild.
    touch src/bin/skyhook/embedded_shims.rs
    cargo build --locked --release --features tui

# Debug builds of every shim, staged into target/shims.
build-shims: (_shims "dev")

# Size-optimised builds of every shim, staged into target/shims.
build-shims-release: (_shims "release")

# Run the test suite with nextest, plus doctests (which nextest does not run).
test:
    cargo nextest run --locked --all-targets --all-features
    cargo test --locked --doc --all-features

# Lint all targets.
lint:
    cargo clippy --locked --all-targets --all-features -- -D warnings

# Format all Rust code.
fmt:
    cargo fmt --all

# Build docs/src into docs/book using the currently installed mdbook.
docs:
    mdbook build docs

# Serve the book locally; mdBook watches for source changes.
docs-serve:
    mdbook serve docs

# Prepare the static GitHub Pages artifact (no Jekyll processing).
pages-build: docs
    touch docs/book/.nojekyll

# Build, then publish committed sources via a temporary worktree; never force-push.
pages-publish remote='origin' branch='pages':
    bash scripts/pages-publish.sh {{ quote(remote) }} {{ quote(branch) }}

# Build every shim type in `mode` (dev or release); add new shim recipes here.
_shims mode:
    if [ {{ quote(mode) }} = dev ]; then \
        just _shims_linux_ssh dev debug; \
    elif [ {{ quote(mode) }} = release ]; then \
        just _shims_linux_ssh shim-release shim-release; \
    else \
        echo "shim mode must be dev or release" >&2; exit 1; \
    fi

# Build the linux-ssh shim for every target in linux_ssh_targets, staged into target/shims.
_shims_linux_ssh profile dir:
    for triple in {{ linux_ssh_targets }}; do \
        cargo zigbuild --locked --profile {{ profile }} --bin linux-ssh \
            --features shim-bin --target "$triple"; \
        install -Dm 755 "target/$triple/{{ dir }}/linux-ssh" "target/shims/linux-ssh-${triple%%-*}"; \
    done
