#!/bin/sh
# Rewrite the flatpak source list from Cargo.lock.
#
# Run after anything that changes Cargo.lock. A stale list does not fail
# loudly - it builds the wrong versions of things - so CI checks it too.
set -eu
cd "$(dirname "$0")/.."

gen=packaging/flatpak-cargo-generator.py
out=packaging/cargo-sources.json

# The generator declares its own dependencies inline (PEP 723), which uv can
# resolve on its own; without uv they have to be installed already.
if command -v uv >/dev/null 2>&1; then
  uv run --quiet "$gen" Cargo.lock -o "$out"
elif python3 -c 'import aiohttp, yaml, tomlkit' 2>/dev/null; then
  python3 "$gen" Cargo.lock -o "$out"
else
  echo "needs uv, or: python3 -m pip install aiohttp PyYAML tomlkit" >&2
  exit 1
fi

echo "$out: $(python3 -c 'import json;print(len(json.load(open("'"$out"'"))))') sources"
