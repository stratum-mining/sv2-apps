use crate::{
    interceptor::MessageDirection,
    mock_roles::{MockUpstream, SniffedDownstream, WithSetup, send_chain_tip},
    sniffer::Sniffer,
    start_jdc, start_sniffer, sv2_tp_config,
    utils::get_available_address,
};
use async_channel::Sender;
use jd_client_sv2::JobDeclaratorClient;
use std::{net::SocketAddr, time::Duration};
use stratum_apps::stratum_core::{
    common_messages_sv2::{MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol},
    mining_sv2::{
        OpenExtendedMiningChannelOwned, OpenExtendedMiningChannelSuccessOwned,
        OpenMiningChannelErrorOwned,
    },
    parsers_sv2::{AnyMessageOwned, MiningOwned},
};

/// A JDC whose Template Provider, Pool and JDS are mocks, the Pool behind a sniffer, so a test
/// decides what every peer sends and observes what JDC sends the Pool. A second Pool/JDS pair is
/// configured after the first, so a fallback from the first shows as JDC setting up the second.
pub struct MockedJdc {
    pub jdc: JobDeclaratorClient,
    pub address: SocketAddr,
    tp: Sender<AnyMessageOwned>,
    pool: Sender<AnyMessageOwned>,
    pub pool_sniffer: Sniffer<'static>,
    pub next_pool_sniffer: Sniffer<'static>,
}

impl MockedJdc {
    /// Starts JDC with its mocked peers.
    pub async fn start() -> Self {
        let tp_address = get_available_address();
        let tp = MockUpstream::new(
            tp_address,
            WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
        )
        .start()
        .await;

        let pool_address = get_available_address();
        let pool = MockUpstream::new(
            pool_address,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;
        let (pool_sniffer, pool_sniffer_address) =
            start_sniffer("jdc-pool", pool_address, false, vec![], None);

        let jds_address = get_available_address();
        MockUpstream::new(
            jds_address,
            WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0),
        )
        .start()
        .await;

        let next_pool_address = get_available_address();
        MockUpstream::new(
            next_pool_address,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;
        let (next_pool_sniffer, next_pool_sniffer_address) =
            start_sniffer("jdc-next-pool", next_pool_address, false, vec![], None);
        let next_jds_address = get_available_address();
        MockUpstream::new(
            next_jds_address,
            WithSetup::yes_with_defaults(Protocol::JobDeclarationProtocol, 0),
        )
        .start()
        .await;

        let (jdc, address, _) = start_jdc(
            &[
                (pool_sniffer_address, jds_address),
                (next_pool_sniffer_address, next_jds_address),
            ],
            sv2_tp_config(tp_address),
            vec![],
            vec![],
            false,
            None,
        );

        Self {
            jdc,
            address,
            tp,
            pool,
            pool_sniffer,
            next_pool_sniffer,
        }
    }

    /// Waits for JDC to fall back from the first Pool/JDS pair and set up the next Pool.
    pub async fn wait_for_fallback(&self) {
        self.next_pool_sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
            )
            .await;
    }

    /// The Template Provider sends future template 1 and the prevhash activating it.
    pub async fn send_chain_tip(&self) {
        send_chain_tip(&self.tp, 1).await;
    }

    /// Waits for the `OpenExtendedMiningChannel` JDC sends the Pool.
    pub async fn upstream_channel_request(&self) -> OpenExtendedMiningChannelOwned {
        loop {
            match self.pool_sniffer.next_message_from_downstream() {
                Some((
                    _,
                    AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)),
                )) => return request,
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    }

    /// The Pool answers JDC's channel request with `success`.
    pub async fn send_upstream_channel_success(
        &self,
        success: OpenExtendedMiningChannelSuccessOwned,
    ) {
        self.pool
            .send(AnyMessageOwned::Mining(
                MiningOwned::OpenExtendedMiningChannelSuccess(success),
            ))
            .await
            .expect("mock pool should send OpenExtendedMiningChannel.Success");
    }

    /// The Pool refuses JDC's channel request `request_id`.
    pub async fn send_upstream_channel_error(&self, request_id: u32) {
        self.pool
            .send(AnyMessageOwned::Mining(
                MiningOwned::OpenMiningChannelError(OpenMiningChannelErrorOwned {
                    request_id,
                    error_code: "unknown-user".try_into().unwrap(),
                }),
            ))
            .await
            .expect("mock pool should send OpenMiningChannel.Error");
    }

    /// Connects a mock downstream to JDC through a sniffer, and returns it once JDC has accepted
    /// it. JDC accepts downstreams only once it holds a future template and a prevhash, so this
    /// also shows the chain tip has been handled.
    pub async fn connect_downstream(&self, identifier: &'static str) -> SniffedDownstream {
        SniffedDownstream::connect(self.address, identifier).await
    }
}
