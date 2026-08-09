#!/usr/bin/env bash
# Build the WordPress image. Everything WordPress-specific is in the Dockerfile
# (FROM php + ADD <url> to fetch WordPress and the SQLite plugin) — this only
# builds+tags the two images into wk's local store:
#   php        <- the PHP interpreter image (needs plugins/php/php.wasm)
#   wordpress  <- FROM php, referenced by the example as image://wordpress
#
# `--network` lets the Dockerfile's ADD <url> fetch at build time. Then:
#   wk run example/wordpress.wk   and open http://localhost:8092
set -euo pipefail
cd "$(dirname "$0")"

WK="${WK:-$(command -v wk || echo ../../target/debug/wk)}"

if [ ! -f ../php/php.wasm ]; then
    echo "plugins/php/php.wasm is missing — build the php plugin first:" >&2
    echo "  (cd ../php && mise run build)" >&2
    exit 1
fi

echo "building the php base image..."
"$WK" images build --tag php ../php/Dockerfile

echo "building the wordpress image (fetches WordPress + the SQLite plugin)..."
"$WK" images build --network --tag wordpress ./Dockerfile

echo "done — the example uses image://wordpress"
