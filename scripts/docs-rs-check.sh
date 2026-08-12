#!/usr/bin/env bash

# Run docs.rs-like documentation builds for one or more crates.
# Arguments:
#   paths to Cargo.toml files
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "Usage: $0 <manifest-path> [<manifest-path> ...]" >&2
  exit 2
fi

for manifest_path in "$@"; do
  if [ ! -f "$manifest_path" ]; then
    echo "Manifest not found: $manifest_path" >&2
    exit 1
  fi

  echo "Building docs for ${manifest_path}"
  cargo +nightly docs-rs --manifest-path="$manifest_path" --offline
done
