#!/usr/bin/env bash
# Cross-compile a static Linux binary from macOS using cargo-zigbuild.
#
# Usage:
#   scripts/build-linux.sh            # x86_64 (default)
#   scripts/build-linux.sh aarch64    # arm64 (e.g. Raspberry Pi, Graviton)
#
# Requires: zig (brew install zig) and cargo-zigbuild (cargo install cargo-zigbuild).
# The rustup target is added automatically if missing.
set -euo pipefail

arch="${1:-x86_64}"
case "$arch" in
  x86_64|aarch64) ;;
  *) echo "error: unsupported arch '$arch' (use x86_64 or aarch64)" >&2; exit 1 ;;
esac

target="${arch}-unknown-linux-musl"
cd "$(dirname "$0")/.."

rustup target list --installed | grep -qx "$target" || rustup target add "$target"

cargo zigbuild --release --target "$target"

bin="target/${target}/release/book-release-tracker"
echo
echo "Built: $bin"
file "$bin"
