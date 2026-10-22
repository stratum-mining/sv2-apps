use async_channel::Sender;
use integration_tests_sv2::{
    POOL_COINBASE_REWARD_ADDRESS,
    interceptor::MessageDirection,
    mock_roles::{MockDownstream, MockUpstream, WithSetup},
    sniffer::Sniffer,
    utils::get_available_address,
};
use jd_client_sv2::{JobDeclaratorClient, config::ConfigJDCMode};
use std::{net::SocketAddr, time::Duration};
use stratum_apps::{
    config_helpers::CoinbaseRewardScript,
    stratum_core::{
        binary_sv2::B0255Owned,
        bitcoin::{
            Amount, TxOut,
            consensus::{deserialize, serialize},
        },
        common_messages_sv2::{MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol},
        job_declaration_sv2::{
            AllocateMiningJobTokenOwned, AllocateMiningJobTokenSuccessOwned,
            MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
        },
        mining_sv2::{
            MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS, MESSAGE_TYPE_SET_CUSTOM_MINING_JOB,
            NewExtendedMiningJobOwned, OpenExtendedMiningChannelOwned,
            OpenExtendedMiningChannelSuccessOwned, SetCustomMiningJobOwned,
        },
        parsers_sv2::{
            AnyMessageOwned, JobDeclarationOwned, MiningOwned, TemplateDistributionOwned,
        },
        template_distribution_sv2::{
            MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS, NewTemplateOwned, SetNewPrevHashOwned,
        },
    },
};

// How long the test waits for a message that must never be sent.
const ABSENCE_WINDOW: Duration = Duration::from_millis(2000);

// What every test template leaves for the coinbase outputs to pay.
const TEMPLATE_VALUE: u64 = 5_000_000_000;

/// Regression coverage for JDC building one coinbase-only custom job per template (issue #753).
///
/// In coinbase-only mode JDC pays for each template with one token, spent on the
/// `SetCustomMiningJob` it sends on its single upstream channel. Every downstream mines that
/// template under that one custom job, so the number of downstreams must not change how many
/// custom jobs reach the Pool: a second one would reuse the token, and the Pool rejects a reused
/// token.
///
/// The test connects three downstreams, two with an extended channel open and one without any
/// channel, then sends a new template. Exactly one `SetCustomMiningJob` must reach the Pool,
/// carrying a token no earlier custom job used, and both downstreams with a channel must receive
/// the template's job.
#[tokio::test]
async fn jdc_coinbase_only_sends_one_custom_job_per_template() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    let first = jdc.connect_with_upstream_channel().await;

    // A second downstream opens its own extended channel on the open upstream channel, and a
    // third only completes its setup, without opening any channel.
    let second = jdc.connect_downstream("jdc-downstream-second").await;
    second.request_extended_channel().await;
    second
        .wait_for(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    let without_channel = jdc
        .connect_downstream("jdc-downstream-without-channel")
        .await;

    let spent_tokens: Vec<B0255Owned> = jdc
        .sent_custom_jobs()
        .into_iter()
        .map(|custom_job| custom_job.token)
        .collect();
    jdc.jds_sniffer.clean_queue(MessageDirection::ToUpstream);
    for downstream in [&first, &second, &without_channel] {
        downstream
            .sniffer
            .clean_queue(MessageDirection::ToDownstream);
    }

    jdc.send_template(template(2, false)).await;

    // One custom job reaches the Pool, paid for with a token of its own, and no second one
    // follows.
    let custom_job = jdc.next_custom_job().await;
    assert!(!spent_tokens.contains(&custom_job.token));
    assert!(
        jdc.pool_sniffer
            .assert_message_not_present(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SET_CUSTOM_MINING_JOB,
                ABSENCE_WINDOW,
            )
            .await,
        "JDC must send one custom job per template, whatever the number of downstreams"
    );

    // Both downstreams with a channel mine the template; the one without a channel gets no job.
    for downstream in [&first, &second] {
        downstream
            .wait_for(MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB)
            .await;
    }
    assert!(!without_channel.sniffer.has_message_type(
        MessageDirection::ToDownstream,
        MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB,
    ));

    jdc.jdc.shutdown().await;
}

/// Regression coverage for the custom job of an activated future template paying the template's
/// remaining reward (issue #810).
///
/// The JDS hands out zero-valued outputs, and JDC fills in what each template leaves for the
/// coinbase. When a prevhash activates a future template, the custom job JDC sends the Pool for it
/// must pay that reward, to the same outputs the downstreams were handed for the template.
#[tokio::test]
async fn jdc_coinbase_only_activated_template_custom_job_pays_the_template_reward() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    let downstream = jdc.connect_with_upstream_channel().await;
    jdc.sent_custom_jobs();
    downstream
        .sniffer
        .clean_queue(MessageDirection::ToDownstream);

    jdc.send_template(template(2, true)).await;
    let downstream_job = downstream.next_extended_job().await;
    jdc.send_prev_hash(2).await;

    let custom_job = jdc.next_custom_job().await;
    assert_pays_template_reward(&custom_job, &downstream_job);

    jdc.jdc.shutdown().await;
}

/// Regression coverage for the custom job JDC sends as its upstream channel opens (issue #810).
///
/// The first downstream to request a channel makes JDC open its upstream channel, and is handed a
/// job for the current chain tip once the channel is open. JDC must send the Pool a custom job
/// for that chain tip at the same time, paying the template's remaining reward to the same
/// outputs, or the downstream's shares on its first job have no upstream job to go to.
#[tokio::test]
async fn jdc_coinbase_only_upstream_channel_opens_with_a_custom_job_for_the_chain_tip() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    let downstream = jdc.connect_with_upstream_channel().await;
    let downstream_job = downstream.next_extended_job().await;

    let custom_job = jdc.next_custom_job().await;
    assert_eq!(custom_job.token, mining_job_token(0));
    assert_pays_template_reward(&custom_job, &downstream_job);

    jdc.jdc.shutdown().await;
}

/// Regression coverage for coinbase-only tokens being kept until JDC can spend them (issue #749).
///
/// A prevhash that activates a future template while JDC has no upstream channel cannot be
/// answered with a custom job: there is no channel to send one on. Such activations must leave
/// JDC's tokens where they are, so that once the upstream channel opens, its custom jobs are paid
/// for with the tokens JDC was handed, oldest first.
#[tokio::test]
async fn jdc_coinbase_only_keeps_its_tokens_until_the_upstream_channel_opens() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    let token_requests = jdc.token_requests(2).await;
    jdc.answer_token_requests(&token_requests).await;
    jdc.wait_for_first_token().await;

    // Two chain tips are activated while no downstream has asked for a channel, so while JDC has
    // no upstream channel.
    jdc.send_chain_tip(1).await;
    jdc.send_chain_tip(2).await;

    // JDC handles what the Template Provider sends ahead of what a downstream sends, so by the
    // time a downstream's channel request makes it ask the Pool for the upstream channel, both
    // prevhashes have been handled.
    let downstream = jdc.connect_downstream("jdc-downstream").await;
    downstream.request_extended_channel().await;
    jdc.pool_sniffer
        .wait_for_message_type(
            MessageDirection::ToUpstream,
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        )
        .await;

    // The custom job sent as the upstream channel opens, and the one for the next template, are
    // paid for with the two tokens JDC was handed.
    jdc.open_upstream_channel().await;
    assert_eq!(jdc.next_custom_job().await.token, mining_job_token(0));
    jdc.send_template(template(3, false)).await;
    assert_eq!(jdc.next_custom_job().await.token, mining_job_token(1));

    jdc.jdc.shutdown().await;
}

/// Regression coverage for work handed out before the JDS's first token answer (issue #896).
///
/// Until the JDS answers a token request, JDC hands templates out with the miner's own outputs:
/// solo work, which never goes upstream. With the upstream channel already open, the chain tip gets
/// no custom job and asks for no token. Once the JDS answers, JDC sends the Template Provider
/// constraints for the JDS's outputs, and the template the chain tip moves to next gets its custom
/// job.
#[tokio::test]
async fn jdc_coinbase_only_chain_tip_before_the_first_token_answer_stays_solo() {
    integration_tests_sv2::start_tracing();
    let jdc = MockedJdc::start().await;
    let token_requests = jdc.token_requests(2).await;
    jdc.send_chain_tip(1).await;
    let downstream = jdc.connect_downstream("jdc-downstream").await;
    downstream.request_extended_channel().await;
    jdc.open_upstream_channel().await;
    downstream
        .wait_for(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
        .await;
    assert!(
        jdc.pool_sniffer
            .assert_message_not_present(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SET_CUSTOM_MINING_JOB,
                ABSENCE_WINDOW,
            )
            .await,
        "JDC must not send solo work upstream"
    );
    assert!(
        jdc.jds_sniffer
            .assert_message_not_present(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_ALLOCATE_MINING_JOB_TOKEN,
                ABSENCE_WINDOW,
            )
            .await,
        "solo work must not ask the JDS for a token"
    );

    jdc.answer_token_requests(&token_requests).await;
    jdc.wait_for_first_token().await;
    jdc.send_chain_tip(2).await;

    assert_eq!(jdc.next_custom_job().await.token, mining_job_token(0));

    jdc.jdc.shutdown().await;
}

// Asserts that `custom_job` pays the whole remaining reward of a `template`, to the same outputs
// as `downstream_job`, built from the same template.
fn assert_pays_template_reward(
    custom_job: &SetCustomMiningJobOwned,
    downstream_job: &NewExtendedMiningJobOwned,
) {
    let outputs: Vec<TxOut> = deserialize(custom_job.coinbase_tx_outputs.as_bytes())
        .expect("custom job outputs must deserialize");
    let paid: u64 = outputs.iter().map(|output| output.value.to_sat()).sum();
    assert_eq!(paid, TEMPLATE_VALUE);
    assert!(
        downstream_job
            .coinbase_tx_suffix
            .as_bytes()
            .windows(custom_job.coinbase_tx_outputs.as_bytes().len())
            .any(|window| window == custom_job.coinbase_tx_outputs.as_bytes()),
        "the custom job and the downstream job must pay the same outputs"
    );
}

// A coinbase-only JDC whose Template Provider, Pool and JDS are mocks, each behind a sniffer, so a
// test decides what every peer sends and observes everything JDC sends back.
struct MockedJdc {
    jdc: JobDeclaratorClient,
    address: SocketAddr,
    tp: Sender<AnyMessageOwned>,
    pool: Sender<AnyMessageOwned>,
    jds: Sender<AnyMessageOwned>,
    tp_sniffer: Sniffer<'static>,
    pool_sniffer: Sniffer<'static>,
    jds_sniffer: Sniffer<'static>,
}

impl MockedJdc {
    async fn start() -> Self {
        let tp_address = get_available_address();
        let tp = MockUpstream::new(
            tp_address,
            WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
        )
        .start()
        .await;
        let (tp_sniffer, tp_sniffer_address) =
            integration_tests_sv2::start_sniffer("jdc-tp", tp_address, false, vec![], None);

        let pool_address = get_available_address();
        let pool = MockUpstream::new(
            pool_address,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;
        let (pool_sniffer, pool_sniffer_address) =
            integration_tests_sv2::start_sniffer("jdc-pool", pool_address, false, vec![], None);

        let jds_address = get_available_address();
        let jds = MockUpstream::new(
            jds_address,
            WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0),
        )
        .start()
        .await;
        let (jds_sniffer, jds_sniffer_address) =
            integration_tests_sv2::start_sniffer("jdc-jds", jds_address, false, vec![], None);

        let (jdc, address, _) = integration_tests_sv2::start_jdc(
            &[(pool_sniffer_address, jds_sniffer_address)],
            integration_tests_sv2::sv2_tp_config(tp_sniffer_address),
            vec![],
            vec![],
            false,
            Some(ConfigJDCMode::CoinbaseOnly),
        );

        Self {
            jdc,
            address,
            tp,
            pool,
            jds,
            tp_sniffer,
            pool_sniffer,
            jds_sniffer,
        }
    }

    // Brings JDC to where it mines through the Pool: two tokens and its outputs from the JDS, a
    // chain tip from the Template Provider handed out with those outputs, and one downstream whose
    // extended channel request opened the upstream channel. Returns that downstream once its
    // channel is open.
    async fn connect_with_upstream_channel(&self) -> MockedDownstream {
        let token_requests = self.token_requests(2).await;
        self.answer_token_requests(&token_requests).await;
        self.wait_for_first_token().await;
        self.send_chain_tip(1).await;
        let first = self.connect_downstream("jdc-downstream-first").await;
        first.request_extended_channel().await;
        self.open_upstream_channel().await;
        first
            .wait_for(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
            .await;
        first
    }

    // Waits for `count` token requests from JDC.
    async fn token_requests(&self, count: usize) -> Vec<AllocateMiningJobTokenOwned> {
        let mut requests = Vec::new();
        while requests.len() < count {
            match self.jds_sniffer.next_message_from_downstream() {
                Some((
                    _,
                    AnyMessageOwned::JobDeclaration(JobDeclarationOwned::AllocateMiningJobToken(
                        request,
                    )),
                )) => requests.push(request),
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
        requests
    }

    // The JDS answers each request with its own token, numbered from 0 in request order. Like a
    // JDS does, it hands out zero-valued outputs for JDC to fill in from each template.
    async fn answer_token_requests(&self, requests: &[AllocateMiningJobTokenOwned]) {
        let coinbase_outputs = serialize(&vec![TxOut {
            value: Amount::from_sat(0),
            script_pubkey: CoinbaseRewardScript::from_descriptor(&format!(
                "addr({POOL_COINBASE_REWARD_ADDRESS})"
            ))
            .expect("pool reward descriptor must be valid")
            .script_pubkey(),
        }]);
        for (token, request) in requests.iter().enumerate() {
            self.jds
                .send(AnyMessageOwned::JobDeclaration(
                    JobDeclarationOwned::AllocateMiningJobTokenSuccess(
                        AllocateMiningJobTokenSuccessOwned {
                            request_id: request.request_id,
                            mining_job_token: mining_job_token(token as u64),
                            coinbase_outputs: coinbase_outputs
                                .clone()
                                .try_into()
                                .expect("serialized coinbase outputs must fit into B064K"),
                        },
                    ),
                ))
                .await
                .expect("mock JDS should send a token");
        }
    }

    // Waits until JDC holds the first token answered. With it, JDC takes the JDS's coinbase
    // outputs in place of its own, and sends the Template Provider their constraints, after the
    // ones for its own outputs it sent on startup.
    async fn wait_for_first_token(&self) {
        for _ in 0..2 {
            self.tp_sniffer
                .wait_for_message_type_and_clean_queue(
                    MessageDirection::ToUpstream,
                    MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS,
                )
                .await;
        }
    }

    // The Template Provider sends future template `template_id` and the prevhash activating it.
    async fn send_chain_tip(&self, template_id: u64) {
        self.send_template(template(template_id, true)).await;
        self.send_prev_hash(template_id).await;
    }

    async fn send_template(&self, template: NewTemplateOwned) {
        self.tp
            .send(AnyMessageOwned::TemplateDistribution(
                TemplateDistributionOwned::NewTemplate(template),
            ))
            .await
            .expect("mock TP should send NewTemplate");
    }

    async fn send_prev_hash(&self, template_id: u64) {
        self.tp
            .send(AnyMessageOwned::TemplateDistribution(
                TemplateDistributionOwned::SetNewPrevHash(SetNewPrevHashOwned {
                    template_id,
                    prev_hash: [template_id as u8; 32].into(),
                    ntime_start: 1_700_000_000,
                    n_bits: 0x1d00ffff,
                    target: [0xff; 32].into(),
                }),
            ))
            .await
            .expect("mock TP should send SetNewPrevHash");
    }

    // The Pool opens the upstream channel JDC asks it for.
    async fn open_upstream_channel(&self) {
        let request = loop {
            match self.pool_sniffer.next_message_from_downstream() {
                Some((
                    _,
                    AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)),
                )) => break request,
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        };
        self.pool
            .send(AnyMessageOwned::Mining(
                MiningOwned::OpenExtendedMiningChannelSuccess(
                    OpenExtendedMiningChannelSuccessOwned {
                        request_id: request.request_id,
                        channel_id: 9,
                        target: [0xff; 32].into(),
                        extranonce_size: request.min_extranonce_size,
                        extranonce_prefix: vec![0; 4]
                            .try_into()
                            .expect("extranonce prefix must fit into B032"),
                        group_channel_id: 1,
                    },
                ),
            ))
            .await
            .expect("mock pool should open the upstream extended channel");
    }

    // Connects a mock downstream to JDC through a sniffer, and returns it once JDC has accepted
    // it. JDC accepts downstreams only once it holds a future template and a prevhash, so this
    // also shows the first chain tip has been handled.
    async fn connect_downstream(&self, identifier: &'static str) -> MockedDownstream {
        let (sniffer, sniffer_address) =
            integration_tests_sv2::start_sniffer(identifier, self.address, false, vec![], None);
        let sender = MockDownstream::new(
            sniffer_address,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;
        let downstream = MockedDownstream { sniffer, sender };
        downstream
            .wait_for(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
            .await;
        downstream
    }

    // Waits for the next custom job JDC sends the Pool. Whatever JDC sent the Pool before it is
    // discarded, and whatever it sent after it stays queued.
    async fn next_custom_job(&self) -> SetCustomMiningJobOwned {
        self.pool_sniffer
            .wait_for_message_type(
                MessageDirection::ToUpstream,
                MESSAGE_TYPE_SET_CUSTOM_MINING_JOB,
            )
            .await;
        loop {
            if let Some((_, AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJob(custom_job)))) =
                self.pool_sniffer.next_message_from_downstream()
            {
                return custom_job;
            }
        }
    }

    // Returns the custom jobs JDC has sent the Pool and the test has not read yet, and discards
    // everything else JDC has sent it.
    fn sent_custom_jobs(&self) -> Vec<SetCustomMiningJobOwned> {
        std::iter::from_fn(|| self.pool_sniffer.next_message_from_downstream())
            .filter_map(|(_, message)| match message {
                AnyMessageOwned::Mining(MiningOwned::SetCustomMiningJob(custom_job)) => {
                    Some(custom_job)
                }
                _ => None,
            })
            .collect()
    }
}

// A mock downstream connected to JDC, behind a sniffer.
struct MockedDownstream {
    sniffer: Sniffer<'static>,
    sender: Sender<AnyMessageOwned>,
}

impl MockedDownstream {
    async fn request_extended_channel(&self) {
        self.sender
            .send(AnyMessageOwned::Mining(
                MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                    request_id: 1,
                    user_identity: "coinbase-only".try_into().unwrap(),
                    nominal_hash_rate: 1_000.0,
                    max_target: [0xff; 32].into(),
                    min_extranonce_size: 0,
                }),
            ))
            .await
            .expect("mock downstream should request an extended channel");
    }

    // Waits for JDC to send this downstream a message of `message_type`.
    async fn wait_for(&self, message_type: u8) {
        self.sniffer
            .wait_for_message_type(MessageDirection::ToDownstream, message_type)
            .await;
    }

    // Waits for the next extended job JDC hands this downstream. Whatever JDC sent it before the
    // job is discarded, and whatever it sent after it stays queued.
    async fn next_extended_job(&self) -> NewExtendedMiningJobOwned {
        self.wait_for(MESSAGE_TYPE_NEW_EXTENDED_MINING_JOB).await;
        loop {
            if let Some((_, AnyMessageOwned::Mining(MiningOwned::NewExtendedMiningJob(job)))) =
                self.sniffer.next_message_from_upstream()
            {
                return job;
            }
        }
    }
}

fn mining_job_token(token: u64) -> B0255Owned {
    token
        .to_le_bytes()
        .to_vec()
        .try_into()
        .expect("u64 token must fit into B0255")
}

// A known-good template, also used by the channel job-factory tests, under `template_id`.
fn template(template_id: u64, future_template: bool) -> NewTemplateOwned {
    NewTemplateOwned {
        template_id,
        future_template,
        version: 536_870_912,
        coinbase_tx_version: 2,
        coinbase_prefix: vec![82, 0]
            .try_into()
            .expect("coinbase prefix must fit into B0255"),
        coinbase_tx_input_sequence: u32::MAX,
        coinbase_tx_value_remaining: TEMPLATE_VALUE,
        coinbase_tx_outputs_count: 1,
        coinbase_tx_outputs: vec![
            0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63, 113, 209,
            222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137, 121, 153, 98, 180,
            139, 235, 216, 54, 151, 78, 140, 249,
        ]
        .try_into()
        .expect("coinbase outputs must fit into B064K"),
        coinbase_tx_locktime: 0,
        merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
    }
}
