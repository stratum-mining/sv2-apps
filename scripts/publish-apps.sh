#!/usr/bin/env bash

# Script to publish the pool applications in this repository
# Usage: ./scripts/publish-pool-apps.sh [cargo publish options]
# Example: ./scripts/publish-pool-apps.sh --dry-run

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$PROJECT_ROOT"

echo "SV2 Applications Publisher"
echo "=========================="
echo "This script will publish the following crates:"
echo "Pool Applications:"
echo "  - pool_sv2 (from ./pool-apps/pool/)"
echo "  - jd_server (from ./pool-apps/jd-server/)"
echo "Miner Applications:"
echo "  - jd_client (from ./miner-apps/jd-client/)"
echo "  - translator_sv2 (from ./miner-apps/translator/)"
echo "Test Utilities:"
echo "  - mining_device (from ./miner-apps/test-utils/mining-device/)"
echo "  - mining_device_sv1 (from ./miner-apps/test-utils/mining-device-sv1/)"
echo ""

# Check if user is logged in to crates.io
if ! cargo login --help > /dev/null 2>&1; then
    echo "❌ Error: cargo is not available"
    exit 1
fi

# Warn about publishing
if [[ ! "$*" =~ --dry-run ]]; then
    echo "⚠️  WARNING: This will publish crates to crates.io"
    echo "   Make sure you have the proper permissions and have logged in with 'cargo login <token>'"
    echo ""
    read -p "Do you want to continue? (y/N): " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        echo "❌ Publishing cancelled"
        exit 1
    fi
fi

echo ""
echo "Starting publication process..."

# Function to publish a crate
publish_crate() {
    local crate_dir="$1"
    local crate_name="$2"
    shift 2  # Remove first two arguments, leaving publish options
    
    echo ""
    echo "📦 Publishing $crate_name from $crate_dir..."
    
    # Change to crate directory
    if ! cd "$crate_dir"; then
        echo "❌ Error: Could not change to directory $crate_dir"
        return 1
    fi
    
    # Run cargo publish with provided arguments
    if cargo publish "$@"; then
        echo "✅ $crate_name published successfully"
        cd "$PROJECT_ROOT"  # Return to project root
        return 0
    else
        local exit_code=$?
        echo "❌ Failed to publish $crate_name (exit code: $exit_code)"
        cd "$PROJECT_ROOT"  # Return to project root
        return $exit_code
    fi
}

# Publish Pool Apps
echo "📦 Publishing Pool Applications..."
publish_crate "pool-apps/pool" "pool_sv2" "$@"
publish_crate "pool-apps/jd-server" "jd_server" "$@"

# Publish Miner Apps  
echo "📦 Publishing Miner Applications..."
publish_crate "miner-apps/jd-client" "jd_client" "$@"
publish_crate "miner-apps/translator" "translator_sv2" "$@"

# Publish Test Utils
echo "📦 Publishing Test Utilities..."
publish_crate "miner-apps/test-utils/mining-device" "mining_device" "$@"
publish_crate "miner-apps/test-utils/mining-device-sv1" "mining_device_sv1" "$@"

echo ""
echo "🎉 All SV2 applications published successfully!"
echo ""
echo "You can now use these crates in other projects:"
echo "Pool Applications:"
echo "  pool_sv2 = \"0.1.3\""
echo "  jd_server = \"0.1.3\""
echo "Miner Applications:"
echo "  jd_client = \"0.1.3\""
echo "  translator_sv2 = \"0.1.3\""
echo "Test Utilities:"
echo "  mining_device = \"0.1.3\""
echo "  mining_device_sv1 = \"0.1.3\"" 