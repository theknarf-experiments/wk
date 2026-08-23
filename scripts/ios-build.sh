#!/usr/bin/env bash
# Build wk for iOS (simulator by default, `--device` for hardware).
#
# The C-based dependencies (ring, aws-lc-sys) need Xcode's clang, and this repo
# puts wasi-sdk's on PATH for plugin builds — that one only knows wasm targets,
# so `cc` fails with "No available targets are compatible with triple
# arm64-apple-ios...". Pin the toolchain per target rather than reordering PATH,
# which would break the plugin builds.
set -euo pipefail

TARGET="aarch64-apple-ios-sim"
[ "${1:-}" = "--device" ] && TARGET="aarch64-apple-ios"

command -v xcrun >/dev/null || { echo "no xcrun — see: mise run ios-check"; exit 1; }
CLANG="$(xcrun -f clang)"
AR="$(xcrun -f ar)"
[ -x "$CLANG" ] || { echo "no Xcode clang — see: mise run ios-check"; exit 1; }

# `cc` reads CC_<target> with dashes as underscores.
key="${TARGET//-/_}"
export "CC_${key}=$CLANG"
export "CXX_${key}=$(xcrun -f clang++)"
export "AR_${key}=$AR"

echo "target   $TARGET"
echo "clang    $CLANG"
echo

# wk-server only: the root binary pulls in the local UI (winit/wgpu windowing),
# which an iOS app links differently — the server is what a device build needs
# to prove first.
cargo build --target "$TARGET" --features pulley -p wk-server "${@:2}"

echo
echo "built wk-server for $TARGET"
