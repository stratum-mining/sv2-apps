
#!/bin/sh

# Pool Apps workspace (builds pool and jd-server)
POOL_WORKSPACE="pool-apps"

# Miner Apps workspace (builds jd-client, translator, and test-utils)  
MINER_WORKSPACE="miner-apps"

# Integration tests workspace
INTEGRATION_WORKSPACE="test/integration-tests"

ALL_WORKSPACES="$POOL_WORKSPACE $MINER_WORKSPACE $INTEGRATION_WORKSPACE"

for workspace in $ALL_WORKSPACES; do
    echo "Executing build on: $workspace"
    cargo +1.75.0 build --manifest-path="$workspace/Cargo.toml" -- 
    if [ $? -ne 0 ]; then
        echo "Build found some errors in: $workspace"
        exit 1
    fi

    echo "Running fmt on: $workspace"
    (cd $workspace && cargo +nightly fmt)
    if [ $? -ne 0 ]; then
        echo "Fmt failed in: $workspace"
        exit 1
    fi
done

echo "build success!"