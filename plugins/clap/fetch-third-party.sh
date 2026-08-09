#!/usr/bin/env bash
# Fetch third-party CLAP plugin sources into examples/ so you can port them with
# the shim. These are NOT redistributed in this repo (individual/unclear
# licenses); the fetched files are git-ignored. After fetching:  ./build.sh <name>
set -euo pipefail
cd "$(dirname "$0")/examples"

# nakst's HelloCLAP tutorial synth — a single-file C++ CLAP instrument, no GUI.
# Tutorial: https://nakst.gitlab.io/tutorial/clap-part-2.html
curl -fsSL https://raw.githubusercontent.com/nakst/cdn/main/clap-tutorial-part-2-plugin.cpp \
    -o nakst-hello.cpp
echo "fetched examples/nakst-hello.cpp  ->  build with: ./build.sh nakst-hello"
