#!/usr/bin/env bash
# Checks this machine for what the shims and release builds need beside the Rust tooling
# `just setup` installs, and names everything missing at once.
set -euo pipefail
zig=${CARGO_ZIGBUILD_ZIG_PATH:-zig}
missing=()
for tool in "$zig" clang clang++ ld.lld llvm-ar llvm-nm llvm-readelf llvm-objcopy llvm-config \
  gn ninja git python3 unshare; do
  command -v "$tool" >/dev/null || missing+=("$tool")
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "shims and release builds need these on PATH: ${missing[*]}" >&2
  exit 1
fi
problems=()
# `zig env` prints the ZON scripts/v8-archive.sh reads from Zig 0.15 on.
zig_version=$("$zig" version)
IFS=. read -r zig_major zig_minor _ <<<"$zig_version"
if ! [[ $zig_major$zig_minor =~ ^[0-9]+$ ]] || { [ "$zig_major" -eq 0 ] && [ "$zig_minor" -lt 15 ]; }; then
  problems+=("Zig 0.15 or newer; $zig is $zig_version")
fi
[ -e "$(clang --rtlib=compiler-rt -print-libgcc-file-name)" ] ||
  problems+=("the compiler-rt builtins of $(command -v clang)")
[ -e "$(llvm-config --libdir)/libclang.so" ] ||
  problems+=("libclang.so in $(llvm-config --libdir)")
# V8's own build passes unstable flags to rustc.
[[ $(rustc -V) == *nightly* ]] ||
  problems+=("a nightly Rust toolchain active in this repository; it is $(rustc -V)")
if [ ${#problems[@]} -gt 0 ]; then
  printf 'release builds need %s\n' "${problems[@]}" >&2
  exit 1
fi
