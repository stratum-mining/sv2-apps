use crate::{
    mocks::{ClientSetupConnection, MockClient, MockServer, ServerSetupConnection},
    start_jdc_with_user_identities, sv2_tp_config,
    utils::get_available_address,
};
use jd_client_sv2::JobDeclaratorClient;
use std::net::SocketAddr;
use stratum_apps::stratum_core::common_messages_sv2::{
    MESSAGE_TYPE_SETUP_CONNECTION, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol,
};

/// A JDC whose Template Provider, Pool and JDS are mocks, so a test decides what every peer sends
/// and observes what JDC sends them: it sends from, and reads, the `tp`, `pool` and `next_pool`
/// mocks. A second Pool/JDS pair is configured after the first, so
/// a fallback from the first shows as JDC setting up the second.
pub struct MockedJdc {
    pub jdc: JobDeclaratorClient,
    pub address: SocketAddr,
    /// The Template Provider. JDC's connection to it is connection `0`.
    pub tp: MockServer,
    /// The Pool of the first pair. JDC's connection to it is connection `0`.
    pub pool: MockServer,
    /// The Pool of the second pair, which JDC only connects to when it falls back from the first.
    pub next_pool: MockServer,
    // The JDS of each pair: nothing reads them, they are held so they stay up with the rest.
    _jds: MockServer,
    _next_jds: MockServer,
}

impl MockedJdc {
    /// Starts JDC with its mocked peers.
    pub async fn start() -> Self {
        Self::start_with_signature("JDC").await
    }

    /// Starts JDC with its mocked peers and `jdc_signature` as the miner tag of its coinbases.
    pub async fn start_with_signature(jdc_signature: &str) -> Self {
        let server = |protocol| async move {
            let address = get_available_address();
            let server =
                MockServer::new(address, Some(ServerSetupConnection::new([(protocol, 0)]))).await;
            (server, address)
        };
        let (tp, tp_address) = server(Protocol::TemplateDistributionProtocol).await;
        let (pool, pool_address) = server(Protocol::MiningProtocol).await;
        let (jds, jds_address) = server(Protocol::JobDeclarationProtocol).await;
        let (next_pool, next_pool_address) = server(Protocol::MiningProtocol).await;
        let (next_jds, next_jds_address) = server(Protocol::JobDeclarationProtocol).await;

        let (jdc, address, _) = start_jdc_with_user_identities(
            &[
                (pool_address, jds_address, "IT_TEST".to_string()),
                (next_pool_address, next_jds_address, "IT_TEST".to_string()),
            ],
            sv2_tp_config(tp_address),
            vec![],
            vec![],
            vec![],
            vec![],
            false,
            None,
            jdc_signature,
        );

        Self {
            jdc,
            address,
            tp,
            pool,
            next_pool,
            _jds: jds,
            _next_jds: next_jds,
        }
    }

    /// Waits for JDC to fall back from the first Pool/JDS pair and set up the next Pool.
    pub async fn wait_for_fallback(&self) {
        self.next_pool
            .wait_for_message_type(0, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;
    }

    /// Connects a mock downstream to JDC, and returns it once JDC has accepted it. JDC accepts
    /// downstreams only once it holds a future template and a prevhash, so this also shows the
    /// chain tip has been handled.
    pub async fn connect_downstream(&self) -> MockClient {
        let downstream = MockClient::new(
            self.address,
            Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
        )
        .await;
        downstream
            .wait_for_message_type(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
            .await;
        downstream
    }
}
