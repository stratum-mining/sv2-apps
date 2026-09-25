#!/bin/sh

# Builds and formats every crate in the workspace.

set -e

echo "Building the workspace"
cargo +1.88.0 build --workspace

echo "Running fmt"
cargo +nightly fmt --all

echo "build success!"
