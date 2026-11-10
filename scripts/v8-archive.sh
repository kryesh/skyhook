#!/usr/bin/env bash
# Builds V8 from source for a release target and prints the directory holding its
# librusty_v8.a and src_binding.rs, for RUSTY_V8_ARCHIVE and RUSTY_V8_SRC_BINDING_PATH.
# The version is the `v8` crate's in Cargo.lock. The build uses the system clang, gn
# and ninja, rustup's toolchain and zig's musl headers, and runs without network, so
# nothing is downloaded but the rusty_v8 source checkout and crates.io sources.
# Usage: scripts/v8-archive.sh <rust target>
set -euo pipefail
target=$1
if [ "$(uname)" != Linux ]; then
  echo "release builds compile V8 on a Linux host only" >&2
  exit 1
fi
# A Rust triple's architecture as GN, Debian and Zig's libc headers name it.
names() {
  case ${1%%-*} in
    x86_64) echo x64 amd64 x86 ;;
    aarch64) echo arm64 arm64 aarch64 ;;
    *)
      echo "no V8 build configuration for architecture ${1%%-*} (of $1)" >&2
      return 1
      ;;
  esac
}
host=$(rustc -vV | sed -n 's/^host: //p')
arch=${target%%-*} host_arch=${host%%-*}
target_names=$(names "$target")
host_names=$(names "$host")
read -r _ target_debian target_headers <<<"$target_names"
read -r host_cpu host_debian _ <<<"$host_names"
zig=${CARGO_ZIGBUILD_ZIG_PATH:-zig}
root=$(cd "$(dirname "$0")/.." && pwd)
cache=$root/target/v8
gn_args='v8_enable_webassembly=false symbol_level=0 exclude_unwind_tables=true'

version=$(cargo metadata --locked --format-version 1 --manifest-path "$root/Cargo.toml" |
  python3 -I -c 'import json, sys
print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "v8"))')
clang_major=$(clang -dumpversion | cut -d. -f1)
out=$cache/$version/$target/$(printf '%s clang-%s' "$gn_args" "$clang_major" | sha256sum | cut -c1-12)
if [ -s "$out/librusty_v8.a" ] && [ -s "$out/src_binding.rs" ]; then
  echo "$out"
  exit
fi
"$root/scripts/build-tools.sh"

src=$cache/rusty_v8-$version
if [ ! -d "$src/.git" ]; then
  rm -rf "$src.partial"
  git clone --depth 1 --branch "v$version" --recurse-submodules --shallow-submodules \
    https://github.com/denoland/rusty_v8 "$src.partial" >&2
  mv "$src.partial" "$src"
fi
required=$(sed -n "s/^CLANG_REVISION = 'llvmorg-\([0-9]*\)-.*/\1/p" "$src/tools/clang/scripts/update.py")
if [ -z "$required" ]; then
  echo "cannot read the clang version V8 $version needs from tools/clang/scripts/update.py" >&2
  exit 1
fi
if [ "$clang_major" -lt "$required" ]; then
  echo "V8 $version needs clang $required or newer; $(command -v clang) is clang $clang_major" >&2
  exit 1
fi

# The checkout pins an older Rust; GN's own Rust needs this repository's (nightly) toolchain.
export RUSTUP_TOOLCHAIN
RUSTUP_TOOLCHAIN=$(cd "$root" && rustup show active-toolchain | cut -d' ' -f1)
rust_sysroot=$(rustc --print sysroot)

# A Chromium-style clang_base_path over the system clang: GN expects compiler-rt
# builtins under unknown-vendor triples, for this host and the target alike.
tools=$cache/tools/clang-$clang_major
runtime=$tools/lib/clang/$clang_major/lib
zig_lib=$("$zig" env | sed -n 's/^ *\.lib_dir = "\(.*\)",$/\1/p')
mkdir -p "$tools/bin" "$runtime/$host_arch-unknown-linux-gnu" "$runtime/$arch-unknown-linux-gnu"
for tool in clang clang++ ld.lld llvm-ar llvm-nm llvm-readelf llvm-objcopy; do
  ln -sfn "$(command -v "$tool")" "$tools/bin/$tool"
done
ln -sfn "$(clang -print-resource-dir)/include" "$tools/lib/clang/$clang_major/include"
ln -sfn "$(clang --rtlib=compiler-rt -print-libgcc-file-name)" \
  "$runtime/$host_arch-unknown-linux-gnu/libclang_rt.builtins.a"
if [ ! -e "$runtime/$arch-unknown-linux-gnu/libclang_rt.builtins.a" ]; then
  # The system compiler-rt covers only this host; zig builds the target's from source.
  "$zig" build-lib "$zig_lib/compiler_rt.zig" -target "$arch-linux-gnu" -O ReleaseFast -fPIC -fstrip \
    --name clang_rt.builtins -femit-bin="$runtime/$arch-unknown-linux-gnu/libclang_rt.builtins.a"
fi

# GN runs bindgen itself; build it from crates.io source at the checkout's version.
bindgen_version=$(sed -n '/^name = "bindgen"$/{n;s/^version = "\(.*\)"$/\1/p}' "$src/Cargo.lock")
bindgen=$cache/tools/bindgen-$bindgen_version
if [ ! -x "$bindgen/bin/bindgen" ]; then
  CARGO_TARGET_DIR=$cache/tools/bindgen-build cargo install --locked --quiet \
    bindgen-cli --version "$bindgen_version" --root "$bindgen" >&2
  rm -rf "$cache/tools/bindgen-build"
fi
mkdir -p "$bindgen/lib"
ln -sfn "$rust_sysroot/bin/rustfmt" "$bindgen/bin/rustfmt"
ln -sfn "$(llvm-config --libdir)/libclang.so" "$bindgen/lib/libclang.so"

# rusty_v8's build script downloads a toolchain it finds missing. These stand-ins say
# what it would download is present: GN uses rustup's Rust (rust_sysroot_absolute) but
# lists the vendored rustc as an input, and the build tools it runs on this host compile
# against this host's own headers and libraries rather than a Debian sysroot.
mkdir -p "$src/third_party/rust-toolchain/bin" "$src/build/linux/debian_sid_$host_debian-sysroot" \
  "$src/build/linux/debian_sid_$target_debian-sysroot"
if [ "$arch" != "$host_arch" ]; then
  # A cross build compiles those host tools with a sysroot: this host's own root.
  ln -sfn / "$src/build/linux/debian_bullseye_$host_debian-sysroot"
fi
(cd "$src" && python3 -I -B -c 'import sys
sys.path.insert(0, "tools")
from v8_deps import deps
toolchain = deps["third_party/rust-toolchain"]
url = next(o for o in toolchain["objects"] if eval(o["condition"], {"host_os": "linux", "host_cpu": sys.argv[1]}))
sys.stdout.write("https://storage.googleapis.com/" + toolchain["bucket"] + "/" + url["object_name"])' "$host_cpu") \
  > "$src/third_party/rust-toolchain/.rusty_v8_version"
ln -sfn "$rust_sysroot/bin/rustc" "$src/third_party/rust-toolchain/bin/rustc"

case $target in
  *-linux-musl)
    # Compile-only: nothing links against this sysroot.
    sysroot=$cache/tools/musl-$arch
    rm -rf "$sysroot"
    mkdir -p "$sysroot/usr/include"
    for headers in any-linux-any "$target_headers-linux-any" generic-musl "$arch-linux-musl"; do
      cp -a "$zig_lib/libc/include/$headers/." "$sysroot/usr/include/"
    done
    export RUSTY_V8_MUSL_SYSROOT=$sysroot
    ;;
esac

(cd "$src" && cargo fetch --locked >&2)
mkdir -p "$cache/tmp"
export V8_FROM_SOURCE=1 CARGO_NET_OFFLINE=true TMPDIR=$cache/tmp CCACHE_DIR=$cache/ccache
export GN=$(command -v gn) NINJA=$(command -v ninja)
export CLANG_BASE_PATH=$tools LIBCLANG_PATH=$(llvm-config --libdir)
export GN_ARGS="$gn_args clang_version=\"$clang_major\" rust_sysroot_absolute=\"$rust_sysroot\" \
rustc_version=\"$(rustc -V | tr -c '[:alnum:].\n-' -)\" rust_bindgen_root=\"$bindgen\""
build=$cache/build/$version
# Without network, any download the build attempts fails it.
(cd "$src" && CARGO_TARGET_DIR=$build unshare -rn cargo build --locked --release --lib --target "$target" >&2)

gn_out=$build/$target/release/build/gn_out
mkdir -p "$out"
cp "$gn_out/src_binding.rs" "$out/src_binding.rs"
cp "$gn_out/obj/librusty_v8.a" "$out/librusty_v8.a.partial"
mv "$out/librusty_v8.a.partial" "$out/librusty_v8.a"
echo "$out"
