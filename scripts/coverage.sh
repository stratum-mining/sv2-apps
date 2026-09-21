#!/bin/bash

set -e  # Exit on error

# Coverage is collected per group of crates so that each group keeps its own
# codecov flag, even though all crates now live in a single workspace.
#
# Each group is also restricted to its own source directory: tarpaulin reports
# every workspace file the selected tests execute, so without the filter a
# group's report would carry the lines of the crates it depends on.
tarpaulin() {
  group_name=$1
  shift
  output_dir="target/tarpaulin-reports/$group_name-coverage"

  echo "Running tarpaulin for $group_name..."
  mkdir -p "$output_dir"
  # Use command-line arguments to ensure correct output location
  cargo +nightly tarpaulin --verbose --all-features "$@" --timeout 120 --out Xml --output-dir "$output_dir"

  # Verify the output file was created
  if [ -f "$output_dir/cobertura.xml" ]; then
    echo "✅ Coverage report created at: $output_dir/cobertura.xml"
  else
    echo "❌ Error: cobertura.xml not found at $output_dir/cobertura.xml"
    exit 1
  fi
}

echo "Running coverage analysis for SV2 Applications..."
echo "================================================="

tarpaulin "bitcoin-core-sv2" -p bitcoin_core_sv2 --include-files 'bitcoin-core-sv2/*'
echo ""

tarpaulin "stratum-apps" -p stratum-apps --include-files 'stratum-apps/*'
echo ""

tarpaulin "pool-apps" -p pool_sv2 -p jd_server_sv2 --include-files 'pool-apps/*'
echo ""

tarpaulin "miner-apps" -p jd_client_sv2 -p translator_sv2 --include-files 'miner-apps/*'

echo ""
echo "✅ Coverage analysis completed."
echo ""
echo "Reports generated under target/tarpaulin-reports/:"
echo "  - bitcoin-core-sv2-coverage/"
echo "  - stratum-apps-coverage/"
echo "  - pool-apps-coverage/"
echo "  - miner-apps-coverage/"
