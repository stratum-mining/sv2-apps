use stratum_apps::stratum_core::{
    common_messages_sv2::{
        ChannelEndpointChangedOwned, ReconnectOwned, SetupConnectionErrorOwned,
        SetupConnectionSuccessOwned,
    },
    handlers_sv2::HandleCommonMessagesFromServerOwnedAsync,
    parsers_sv2::Tlv,
};
use tracing::{error, info};

use crate::{
    error::{self, PoolError, PoolErrorKind},
    template_receiver::sv2_tp::Sv2Tp,
};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleCommonMessagesFromServerOwnedAsync for Sv2Tp {
    type Error = PoolError<error::TemplateProvider>;

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        Ok(vec![])
    }

    async fn handle_setup_connection_success(
        &mut self,
        _server_id: Option<usize>,
        msg: SetupConnectionSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!(
            "Received `SetupConnectionSuccess` from TP: version={}, flags={:b}",
            msg.used_version, msg.flags
        );
        Ok(())
    }

    async fn handle_channel_endpoint_changed(
        &mut self,
        _server_id: Option<usize>,
        msg: ChannelEndpointChangedOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!(
            "Received ChannelEndpointChanged with channel id: {}",
            msg.channel_id
        );
        Err(PoolError::shutdown(PoolErrorKind::ChangeEndpoint))
    }

    async fn handle_reconnect(
        &mut self,
        _server_id: Option<usize>,
        msg: ReconnectOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {:?}", msg);
        Ok(())
    }

    async fn handle_setup_connection_error(
        &mut self,
        _server_id: Option<usize>,
        msg: SetupConnectionErrorOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        error!(
            "Received `SetupConnectionError` from TP with error code {}",
            std::str::from_utf8(msg.error_code.as_ref()).unwrap_or("unknown error code")
        );
        Err(PoolError::shutdown(PoolErrorKind::SetupConnectionError))
    }
}
