# Use the installed Rust toolchain, Zig, and just; `just setup` installs the cargo tools.
set shell := ["bash", "-euo", "pipefail", "-c"]
# Evaluate native_target only for recipes that use it, so the rest run without rustc.
set lazy

# List common development and documentation commands.
default:
    @just --list

# Remote shim targets; each is staged as target/shims/<os>-<arch>.
shim_targets := "x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-pc-windows-gnu"

# Cross-compiled release builds route C and C++ allocations through mimalloc as well as Rust's.
release_features := "--features mimalloc/override"
# This machine's own target, the default for release builds. It is linked by the system
# toolchain against the system C library, so it uses that library's name resolution, and builds
# without release_features: with the override, a binary dynamically linked against glibc crashes
# in `free` at startup.
native_target := `rustc -vV | sed -n 's/^host: //p'`

# Install Rust-side tooling and the shim targets, which include the static release targets, then
# check for the compilers and build tools that are installed separately.
setup:
    rustup component add clippy rustfmt
    rustup target add {{ shim_targets }}
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

# Release applications for `target` (default: native; <arch>-unknown-linux-musl is static), embedding target/shims.
build-release target=native_target:
    mkdir -p target/shims
    v8=$(scripts/v8-archive.sh {{ quote(target) }}); \
    RUSTY_V8_ARCHIVE="$v8/librusty_v8.a" RUSTY_V8_SRC_BINDING_PATH="$v8/src_binding.rs" \
        cargo {{ if target == native_target { "build" } else { "zigbuild " + release_features } }} --locked --release -p skyhook-apps --target {{ quote(target) }}

# Debug builds of every shim, staged into target/shims.
build-shims: (_shims "dev")

# Size-optimised builds of every shim, staged into target/shims.
build-shims-release: (_shims "release")

# Release shims, then release applications for `target` that embed them.
build-full target=native_target: build-shims-release (build-release target)

# Build everything for `target` (default: native) and install both applications into ~/.local/bin.
install target=native_target: (build-full target)
    install -Dm 755 target/{{ target }}/release/skyhook ~/.local/bin/skyhook
    install -Dm 755 target/{{ target }}/release/skyhook-security ~/.local/bin/skyhook-security

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
        install -CDm 755 "target/$triple/{{ dir }}/shim$exe" "target/shims/$os-${triple%%-*}"; \
    done
