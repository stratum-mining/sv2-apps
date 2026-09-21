# SV2 Integration Tests

This library provides tools for building integration tests for Stratum V2 roles.

Additionally, the library includes pre-defined integration tests for various Stratum V2
roles/scenarios, which can be found in the `tests` folder.

Each role has a `start_[role_name]` function that can be called to run the respective role.

To assert the behavior of a role or the messages it exchanges with other roles, you can use the `Sniffer` module to listen to the messages between roles.

For examples on how to use the `Sniffer` helper, check out the `sniffer_integration.rs` module or
other tests in the `tests` folder.

All tests run in either regtest or signet network.

Bitcoin Core binaries are downloaded from https://bitcoincore.org/bin/ and the
Template provider (sv2-tp) binaries from https://github.com/stratum-mining/sv2-tp/releases.

Bitcoin Core runs via IPC, and sv2-tp provides Stratum V2 template distribution. These are the only
external dependencies in our tests.

## Running Tests Instructions

To run pre defined integration tests, use the following command:

```bash
$ git clone git@github.com:stratum-mining/stratum.git
$ cargo test -p integration_tests_sv2 --verbose --test '*' -- --nocapture
```

Note: during the execution of the tests, the `template-provider` directory holds the downloaded
binaries (Bitcoin Core and sv2-tp), while test data directories are created in the system temp
directory.

## Standalone `mining_device` Binary

The embedded `integration_tests_sv2::mining_device` module also exposes a standalone executable
binary again.

To build it:

```bash
cargo build -p integration_tests_sv2 --release --bin mining_device
```

The executable will be available at `target/release/mining_device`, relative to the repository root.

To run it directly through Cargo:

```bash
cargo run -p integration_tests_sv2 --release --bin mining_device -- \
  --address-pool 127.0.0.1:3333 \
  --id-device test
```

## Writing Custom Integration Tests

 - To write your own integration tests using this library, you can install the library as follows:

```sh
$ cargo add sv2-integration-tests --git
```

Then, create your own test by using the library as shown below:

```rust
use sv2_integration_tests::{Sniffer, template_provider, interceptor::MessageDirection};
use stratum_apps::stratum_core::{
    common_messages_sv2::{Protocol, SetupConnection, *},
    mining_sv2::*,
    parsers_sv2::{AnyMessage, CommonMessages, Mining, TemplateDistribution},
    template_distribution_sv2::*,
};

struct MyCustomPool;

#[tokio::test]
async fn success_pool_template_provider_connection() {
    let (_tp, tp_addr) = template_provider::start_template_provider(None, template_provider::DifficultyLevel::Low);
    let (sniffer, sniffer_addr) = start_sniffer("", tp_addr, true, vec![], None);
    // From the Pool perspective, `sniffer_addr` is the address of the Template Provider
    // The Sniffer will sit between the Pool and the Template Provider
    let _ = MyCustomPool::start(sniffer_addr).await;
    // here we assert that the downstream(pool in this case) have sent `SetupConnection` message
    // with the correct parameters, protocol, flags, min_version and max_version.  Note that the
    // macro can take any number of arguments after the message argument, but the order is
    // important where a property should be followed by its value.
    sniffer
        .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        .await;
    assert_common_message!(
        &sniffer.next_message_from_downstream(),
        SetupConnection,
        protocol,
        Protocol::TemplateDistributionProtocol,
        flags,
        0,
        min_version,
        2,
        max_version,
        2
    );
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        )
        .await;
    assert_common_message!(
        &sniffer.next_message_from_upstream(),
        SetupConnectionSuccess
    );
    sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS,
        )
        .await;
    assert_tp_message!(
        &sniffer.next_message_from_downstream(),
        CoinbaseOutputConstraints
    );
    sniffer
        .wait_for_message_type(MessageDirection::ToDownstream, MESSAGE_TYPE_NEW_TEMPLATE)
        .await;
    assert_tp_message!(&sniffer.next_message_from_upstream(), NewTemplate);
    sniffer
        .wait_for_message_type(
            MessageDirection::ToDownstream,
            MESSAGE_TYPE_SET_NEW_PREV_HASH,
        )
        .await;
    assert_tp_message!(sniffer.next_message_from_upstream(), SetNewPrevHash);
}
```

## License
MIT OR Apache-2.0
