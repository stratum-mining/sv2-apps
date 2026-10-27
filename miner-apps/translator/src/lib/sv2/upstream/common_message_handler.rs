use crate::{
    error::{self, TproxyError, TproxyErrorKind},
    sv2::Upstream,
    utils::ReconnectRequest,
};
use stratum_apps::stratum_core::{
    common_messages_sv2::{
        ChannelEndpointChangedOwned, ReconnectOwned, SetupConnectionErrorOwned,
        SetupConnectionSuccessOwned,
    },
    handlers_sv2::HandleCommonMessagesFromServerOwnedAsync,
    parsers_sv2::Tlv,
};
use tracing::{debug, info, warn};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleCommonMessagesFromServerOwnedAsync for Upstream {
    type Error = TproxyError<error::Upstream>;

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        Ok(vec![])
    }

    async fn handle_setup_connection_error(
        &mut self,
        _server_id: Option<usize>,
        msg: SetupConnectionErrorOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        debug!("Received: {}", msg);
        let error_code = msg.error_code.as_utf8_or_hex();
        warn!(
            error_code = %error_code,
            "SetupConnection rejected by upstream; preparing fallback"
        );
        Err(TproxyError::fallback(TproxyErrorKind::SetupConnectionError))
    }

    async fn handle_setup_connection_success(
        &mut self,
        _server_id: Option<usize>,
        msg: SetupConnectionSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        debug!("Received: {}", msg);
        info!(
            event = "SetupConnectionSuccess",
            address = %self.address,
            "Upstream SV2 handshake completed"
        );
        Ok(())
    }

    async fn handle_channel_endpoint_changed(
        &mut self,
        _server_id: Option<usize>,
        msg: ChannelEndpointChangedOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        debug!("Received: {}", msg);
        // Extension state must be reset and negotiated again. Reconnecting to the same upstream
        // does both, and keeps negotiation a one-time step per connection.
        Err(TproxyError::reconnect(ReconnectRequest::EndpointChanged {
            channel_id: msg.channel_id,
            upstream: self.entry.clone(),
        }))
    }

    async fn handle_reconnect(
        &mut self,
        _server_id: Option<usize>,
        msg: ReconnectOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        debug!("Received: {}", msg);
        // A host tProxy cannot use is handled like an unreachable one: tProxy moves on to the
        // configured upstreams.
        let new_host = std::str::from_utf8(msg.new_host.as_bytes())
            .ok()
            .filter(|host| !host.chars().any(|c| c.is_whitespace() || c.is_control()))
            .ok_or(TproxyError::fallback(TproxyErrorKind::InvalidReconnectHost))?;

        // The spec forbids reconnecting to a server signed by a different authority key, so the
        // new endpoint keeps this upstream's key, as well as its user identity.
        let mut upstream = self.entry.clone();
        if !new_host.is_empty() {
            upstream.host = new_host.to_string();
        }
        if msg.new_port != 0 {
            upstream.port = msg.new_port;
        }
        upstream.tried_or_flagged = false;
        Err(TproxyError::reconnect(ReconnectRequest::Redirect(upstream)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        error::{Action, LoopControl},
        sv2::upstream::{PROTOCOL_RECONNECT_MIN_INTERVAL, ProtocolReconnect, UpstreamIo},
        utils::UpstreamEntry,
    };
    use async_channel::unbounded;
    use std::{
        str::FromStr,
        time::{Duration, Instant},
    };
    use stratum_apps::{
        key_utils::Secp256k1PublicKey, stratum_core::binary_sv2::Str0255Owned, sync::SharedLock,
    };
    use tokio_util::sync::CancellationToken;

    fn upstream_entry() -> UpstreamEntry {
        UpstreamEntry {
            host: "pool.example".to_string(),
            port: 3333,
            authority_pubkey: Secp256k1PublicKey::from_str(
                "9bDuixKmZqAJnrmP746n8zU1wyAQRrus7th9dxnkPg6RzQvCnan",
            )
            .unwrap(),
            tried_or_flagged: false,
            user_identity: "miner".to_string(),
        }
    }

    fn upstream() -> Upstream {
        let (_upstream_inbound_sender, upstream_receiver) = unbounded();
        let (upstream_sender, _upstream_outbound_receiver) = unbounded();
        let (channel_manager_sender, _channel_manager_inbound_receiver) = unbounded();
        let (_channel_manager_outbound_sender, channel_manager_receiver) = unbounded();
        Upstream {
            upstream_io: UpstreamIo::new(
                upstream_receiver,
                upstream_sender,
                channel_manager_sender,
                channel_manager_receiver,
            ),
            required_extensions: vec![],
            address: "127.0.0.1:3333".parse().unwrap(),
            entry: upstream_entry(),
            protocol_reconnect: SharedLock::new(ProtocolReconnect::default()),
        }
    }

    fn redirect() -> ReconnectRequest {
        ReconnectRequest::Redirect(upstream_entry())
    }

    fn reconnect(new_host: Str0255Owned, new_port: u16) -> ReconnectOwned {
        ReconnectOwned { new_host, new_port }
    }

    fn reconnect_request(error: TproxyError<error::Upstream>) -> ReconnectRequest {
        assert!(matches!(error.action, Action::Reconnect));
        let TproxyErrorKind::ReconnectRequested(request) = error.kind else {
            panic!("expected a requested reconnect");
        };
        *request
    }

    fn requested_upstream(error: TproxyError<error::Upstream>) -> UpstreamEntry {
        let ReconnectRequest::Redirect(upstream) = reconnect_request(error) else {
            panic!("expected a redirect");
        };
        upstream
    }

    #[tokio::test]
    async fn reconnect_keeps_the_authority_key_and_user_identity() {
        let mut upstream = upstream();

        let requested = requested_upstream(
            upstream
                .handle_reconnect(
                    None,
                    reconnect("other.example".try_into().unwrap(), 4444),
                    None,
                )
                .await
                .unwrap_err(),
        );

        assert_eq!(requested.host, "other.example");
        assert_eq!(requested.port, 4444);
        assert_eq!(
            requested.authority_pubkey.into_bytes(),
            upstream_entry().authority_pubkey.into_bytes()
        );
        assert_eq!(requested.user_identity, "miner");
    }

    #[tokio::test]
    async fn reconnect_without_host_or_port_targets_the_current_upstream() {
        let mut upstream = upstream();

        let requested = requested_upstream(
            upstream
                .handle_reconnect(None, reconnect("".try_into().unwrap(), 0), None)
                .await
                .unwrap_err(),
        );

        assert_eq!(requested.host, "pool.example");
        assert_eq!(requested.port, 3333);
    }

    #[tokio::test]
    async fn reconnect_to_an_unusable_host_falls_back() {
        let mut upstream = upstream();

        for new_host in [
            Str0255Owned::new(vec![0xff, 0xfe]).unwrap(),
            "pool .example".try_into().unwrap(),
        ] {
            let error = upstream
                .handle_reconnect(None, reconnect(new_host, 4444), None)
                .await
                .unwrap_err();
            assert!(matches!(error.action, Action::Fallback));
            assert!(matches!(error.kind, TproxyErrorKind::InvalidReconnectHost));
        }
    }

    #[tokio::test]
    async fn channel_endpoint_changed_reconnects_to_the_current_upstream() {
        let mut upstream = upstream();

        let request = reconnect_request(
            upstream
                .handle_channel_endpoint_changed(
                    None,
                    ChannelEndpointChangedOwned { channel_id: 7 },
                    None,
                )
                .await
                .unwrap_err(),
        );
        let ReconnectRequest::EndpointChanged {
            channel_id,
            upstream: requested,
        } = request
        else {
            panic!("expected an endpoint change");
        };
        assert_eq!(channel_id, 7);
        assert_eq!(requested.host, "pool.example");
        assert_eq!(requested.port, 3333);
    }

    #[test]
    fn a_reconnect_hands_the_endpoint_to_the_runtime_and_ends_the_upstream() {
        let upstream = upstream();
        let cancellation_token = CancellationToken::new();
        let fallback_token = CancellationToken::new();
        let mut requested = upstream_entry();
        requested.host = "other.example".to_string();

        let control = upstream.handle_error_action(
            "test",
            &TproxyError::reconnect(ReconnectRequest::Redirect(requested)),
            &cancellation_token,
            &fallback_token,
        );

        assert_eq!(control, LoopControl::Break);
        assert!(fallback_token.is_cancelled());
        assert!(!cancellation_token.is_cancelled());
        assert_eq!(
            upstream
                .protocol_reconnect
                .with(|protocol_reconnect| protocol_reconnect.requested.clone())
                .unwrap()
                .map(|request| request.into_upstream().host),
            Some("other.example".to_string())
        );
    }

    #[test]
    fn requests_are_followed_at_most_once_per_interval() {
        let now = Instant::now();
        let mut protocol_reconnect = ProtocolReconnect::default();

        assert!(protocol_reconnect.record(redirect(), now));
        assert!(protocol_reconnect.take(now).is_some());
        // Ignored requests do not restart the interval.
        for later in [
            Duration::from_secs(120),
            PROTOCOL_RECONNECT_MIN_INTERVAL - Duration::from_secs(1),
        ] {
            assert!(!protocol_reconnect.record(redirect(), now + later));
        }
        assert!(protocol_reconnect.record(redirect(), now + PROTOCOL_RECONNECT_MIN_INTERVAL));
    }

    #[test]
    fn a_dropped_request_does_not_start_the_interval() {
        let now = Instant::now();
        let mut protocol_reconnect = ProtocolReconnect::default();

        assert!(protocol_reconnect.record(redirect(), now));
        protocol_reconnect.discard();
        assert!(protocol_reconnect.record(redirect(), now + Duration::from_secs(1)));
    }

    #[test]
    fn a_reconnect_within_the_interval_is_ignored_and_keeps_the_upstream() {
        let upstream = upstream();
        let cancellation_token = CancellationToken::new();
        let first_fallback_token = CancellationToken::new();
        assert_eq!(
            upstream.handle_error_action(
                "test",
                &TproxyError::reconnect(redirect()),
                &cancellation_token,
                &first_fallback_token,
            ),
            LoopControl::Break
        );
        // The runtime takes the request while following it.
        upstream
            .protocol_reconnect
            .with(|protocol_reconnect| protocol_reconnect.take(Instant::now()))
            .unwrap();

        let second_fallback_token = CancellationToken::new();
        assert_eq!(
            upstream.handle_error_action(
                "test",
                &TproxyError::reconnect(redirect()),
                &cancellation_token,
                &second_fallback_token,
            ),
            LoopControl::Continue
        );
        assert!(!second_fallback_token.is_cancelled());
        assert!(
            upstream
                .protocol_reconnect
                .with(|protocol_reconnect| protocol_reconnect.requested.is_none())
                .unwrap()
        );
    }
}
