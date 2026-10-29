#!/bin/sh

# Runs clippy, the test suite and the formatter over every crate in the
# workspace.
#
# Clippy and the tests run one crate at a time, the same way CI does:
# `--workspace` would unify `stratum-apps` features across every member, so a
# change relying on a feature that only another app enables would pass here
# and still fail once the app is compiled alone.

set -e

APPS="pool_sv2 jd_server_sv2 jd_client_sv2 translator_sv2"

echo "Running script regression tests"
python3 -B -m unittest discover -s scripts/tests -v

echo "Executing clippy"
cargo +1.88.0 clippy -p stratum-apps --all-features -- -D warnings -A dead-code
for crate in $APPS bitcoin_core_sv2 integration_tests_sv2; do
    cargo +1.88.0 clippy -p "$crate" -- -D warnings -A dead-code
done

echo "Running tests"
cargo +1.88.0 test -p stratum-apps --all-features
for crate in $APPS bitcoin_core_sv2 integration_tests_sv2; do
    cargo +1.88.0 test -p "$crate"
done

echo "Running fmt"
cargo +nightly fmt --all

echo "Clippy success, all tests passed!"
