#!/usr/bin/env bash
# Publish wk's bundled plugins to an OCI registry as a known set for testing the
# `oci://` dependency path end to end. There's no good public set of components
# that run in wk (most published wasm is WIT packages or wasi:http servers), so
# we host our own — including genuinely recompiled software (kilo, a C editor).
#
# With the local registry from compose.yml:
#   docker compose up -d
#   ./scripts/publish-known-set.sh
# Then, in a scratch project:
#   wk init demo && wk add oci://localhost:5001/wsh:1.0 && wk run
#
# Override REG / VER / WK via env (e.g. REG=ghcr.io/<you> to publish publicly).
set -euo pipefail
cd "$(dirname "$0")/.."

REG="${REG:-localhost:5001}"
VER="${VER:-1.0}"
WK="${WK:-./target/debug/wk}"

# Plugins must be built first: `cargo component build` in each plugins/* dir,
# and `plugins/kilo/build.sh` for kilo.
for plugin in triangle paint file_demo piano synth wsh kilo; do
    "$WK" publish "$plugin" "$REG/$plugin:$VER"
done

echo "published known set to $REG (tag $VER)"
