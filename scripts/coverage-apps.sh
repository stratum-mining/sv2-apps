#!/bin/bash

tarpaulin() {
  workspace_name=$1
  output_dir="target/tarpaulin-reports/$workspace_name-coverage"
  mkdir -p "$output_dir"
  cargo +nightly tarpaulin --verbose --out Xml --output-dir "$output_dir" --all-features
}

echo "Running coverage analysis for SV2 Applications..."
echo "================================================="

# Pool Apps Coverage (includes pool and jd-server)
echo ""
echo "Running coverage for pool-apps workspace..."
cd pool-apps || exit 1
tarpaulin "pool-apps"
cd - > /dev/null || exit 1

# Miner Apps Coverage (includes jd-client, translator, and test-utils)
echo ""
echo "Running coverage for miner-apps workspace..."
cd miner-apps || exit 1
tarpaulin "miner-apps"
cd - > /dev/null || exit 1

# Integration Tests Coverage
echo ""
echo "Running coverage for integration tests..."
cd test/integration-tests || exit 1
tarpaulin "integration-tests"
cd - > /dev/null || exit 1

echo ""
echo "✅ Coverage analysis completed for all available workspaces."
echo ""
echo "Reports generated:"
echo "  - pool-apps/target/tarpaulin-reports/pool-apps-coverage/"
echo "  - miner-apps/target/tarpaulin-reports/miner-apps-coverage/"
echo "  - test/integration-tests/target/tarpaulin-reports/integration-tests-coverage/"