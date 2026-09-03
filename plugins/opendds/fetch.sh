#!/usr/bin/env bash
# Fetch upstream OpenDDS at the pinned tag. ACE/TAO is NOT fetched here —
# OpenDDS's own configure script downloads it into src/OpenDDS/ACE_TAO, and
# letting it do so is what keeps the ACE and TAO versions in step with the
# OpenDDS release (acetao.ini is upstream's own table of that pairing).
#
# Idempotent: a checkout already at the pinned tag is left alone.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

if [ -d "$SRC/.git" ]; then
  have="$(git -C "$SRC" describe --tags --exact-match 2>/dev/null || echo '')"
  if [ "$have" = "$OPENDDS_TAG" ]; then
    echo "opendds: src/OpenDDS already at $OPENDDS_TAG"
    exit 0
  fi
  echo "opendds: src/OpenDDS is at '${have:-unknown}', wanted $OPENDDS_TAG" >&2
  echo "         remove it and re-run to re-fetch" >&2
  exit 1
fi

log "cloning OpenDDS $OPENDDS_TAG"
mkdir -p "$(dirname "$SRC")"
git clone --depth 1 --branch "$OPENDDS_TAG" "$OPENDDS_REPO" "$SRC"
# rapidjson is a submodule; OpenDDS uses it for JSON sample serialization and
# the Wireshark dissector. Cheap, and configure looks for it.
git -C "$SRC" submodule update --init --depth 1 tools/rapidjson

echo "opendds: fetched $OPENDDS_TAG into $SRC"
