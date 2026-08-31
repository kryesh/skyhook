#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
command -v cross >/dev/null 2>&1 || {
    echo "cross is required: cargo install cross" >&2
    exit 1
}

mkdir -p "$root/dist/shims"
for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    case "$target" in
        x86_64-*) platform=x86_64-linux ;;
        aarch64-*) platform=aarch64-linux ;;
    esac
    cross build --manifest-path "$root/Cargo.toml" --release --bin skyhook-shim --target "$target" --no-default-features
    source_file="$root/target/$target/release/skyhook-shim"
    destination="$root/dist/shims/skyhook-shim-$platform"
    cp "$source_file" "$destination"
    chmod 755 "$destination"
    if command -v readelf >/dev/null 2>&1 && readelf -l "$destination" | grep -q 'Requesting program interpreter'; then
        echo "$destination is dynamically linked" >&2
        exit 1
    fi
done

cargo build --manifest-path "$root/Cargo.toml" --release --bin skyhook
