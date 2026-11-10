# Use the installed Rust toolchain, Zig, and just; `just setup` installs the cargo tools.
set shell := ["bash", "-euo", "pipefail", "-c"]
# Evaluate host_target only for recipes that use it, so the rest work on an OS it does not map.
set lazy

# List common development and documentation commands.
default:
    @just --list

# Remote shim targets; each is staged as target/shims/<os>-<arch>.
shim_targets := "x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-pc-windows-gnu"

# Release builds route C and C++ allocations through mimalloc as well as Rust's.
release_features := "--features mimalloc/override"
# Default release target for this machine: releases are static musl builds for Linux.
host_target := if os() == "linux" { arch() + "-unknown-linux-musl" } else { error("release builds need a Linux host, not " + os()) }

# Install Rust-side tooling, shim targets, and the host release target, then check for the
# compilers and build tools that are installed separately.
setup:
    rustup component add clippy rustfmt
    rustup target add {{ shim_targets }} {{ host_target }}
    cargo install --locked cargo-zigbuild cargo-nextest mdbook
    scripts/build-tools.sh

# Debug CLI; reads whatever is in target/shims live from disk.
[positional-arguments]
run *args:
    cargo run --locked -p skyhook-apps --bin skyhook -- "$@"

# Debug security companion; reads whatever is in target/shims live from disk.
[positional-arguments]
run-security *args:
    cargo run --locked -p skyhook-apps --bin skyhook-security -- "$@"

# Debug applications; read whatever is in target/shims live from disk.
build:
    cargo build --locked -p skyhook-apps

# Static release applications for `target`; embed whatever is in target/shims at compile time.
build-release target=host_target:
    # rust-embed cannot see newly added files; force the embedding module to rebuild.
    touch crates/apps/src/shims.rs
    v8=$(scripts/v8-archive.sh {{ quote(target) }}); \
    RUSTY_V8_ARCHIVE="$v8/librusty_v8.a" RUSTY_V8_SRC_BINDING_PATH="$v8/src_binding.rs" \
        cargo zigbuild --locked --release -p skyhook-apps --target {{ quote(target) }} {{ release_features }}

# Debug builds of every shim, staged into target/shims.
build-shims: (_shims "dev")

# Size-optimised builds of every shim, staged into target/shims.
build-shims-release: (_shims "release")

# Release shims, then release applications for `target` that embed them.
build-full target=host_target: build-shims-release (build-release target)

# Build everything for this machine and install both applications into ~/.local/bin.
install: build-full
    install -Dm 755 target/{{ host_target }}/release/skyhook ~/.local/bin/skyhook
    install -Dm 755 target/{{ host_target }}/release/skyhook-security ~/.local/bin/skyhook-security

# Run the test suite with nextest.
test:
    cargo nextest run --locked --workspace --all-targets

# Lint all targets.
lint:
    cargo clippy --locked --workspace --all-targets -- -D warnings

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

# Build the shims in `mode` (dev or release).
_shims mode:
    if [ {{ quote(mode) }} = dev ]; then \
        just _shim_targets dev debug; \
    elif [ {{ quote(mode) }} = release ]; then \
        just _shim_targets shim-release shim-release {{ quote(release_features) }}; \
    else \
        echo "shim mode must be dev or release" >&2; exit 1; \
    fi

# Build the shim for every target in shim_targets, staged into target/shims.
_shim_targets profile dir features="":
    for triple in {{ shim_targets }}; do \
        os="${triple#*-*-}"; os="${os%%-*}"; \
        exe=""; [ "$os" = windows ] && exe=.exe; \
        cargo zigbuild --locked --profile {{ profile }} -p skyhook-shim --target "$triple" {{ features }}; \
        install -Dm 755 "target/$triple/{{ dir }}/shim$exe" "target/shims/$os-${triple%%-*}"; \
    done
