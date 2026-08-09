#!/usr/bin/env bash
# Builds the native library for one target out of a composed Codex checkout.
#
# Usage: build.sh <codex-checkout> <rust-target> <rid> <out-dir>
# The artifact is named after the .NET runtime identifier, e.g. codex_ffi-osx-arm64.dylib.
set -euo pipefail

tree="${1:?usage: build.sh <codex-checkout> <rust-target> <rid> <out-dir>}"
target="${2:?missing rust target}"
rid="${3:?missing rid}"
outdir="${4:?missing out dir}"

tree="$(cd "$tree" && pwd)"
mkdir -p "$outdir"
outdir="$(cd "$outdir" && pwd)"

[[ -f "$tree/codex-rs/ffi/Cargo.toml" ]] || {
    echo "checkout is not composed, run scripts/compose.sh first: $tree" >&2
    exit 1
}

# Both commands must run inside the workspace: rustup resolves the toolchain from
# the current directory, so building from anywhere else would use the default
# toolchain and miss the target installed here.
(
    cd "$tree/codex-rs"
    rustup target add "$target"
    cargo build --release --lib --package codex-ffi --target "$target"
)

built="$tree/codex-rs/target/$target/release"

case "$rid" in
    osx-*) source="$built/libcodex_ffi.dylib"; artifact="codex_ffi-$rid.dylib" ;;
    linux-*) source="$built/libcodex_ffi.so"; artifact="codex_ffi-$rid.so" ;;
    win-*) source="$built/codex_ffi.dll"; artifact="codex_ffi-$rid.dll" ;;
    *) echo "unknown rid: $rid" >&2; exit 2 ;;
esac

cp "$source" "$outdir/$artifact"
echo "$outdir/$artifact"
