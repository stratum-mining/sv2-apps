use crate::{
    error::{self, TproxyError, TproxyErrorKind},
    sv2::Upstream,
};
use stratum_apps::stratum_core::{
    common_messages_sv2::{
        ChannelEndpointChangedOwned, ReconnectOwned, SetupConnectionErrorOwned,
        SetupConnectionSuccessOwned,
    },
    handlers_sv2::HandleCommonMessagesFromServerOwnedAsync,
    parsers_sv2::Tlv,
};
use tracing::{error, info};

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
        error!("Received: {:?}", msg);
        Err(TproxyError::fallback(TproxyErrorKind::SetupConnectionError))
    }

    async fn handle_setup_connection_success(
        &mut self,
        _server_id: Option<usize>,
        msg: SetupConnectionSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {:?}", msg);
        Ok(())
    }

    async fn handle_channel_endpoint_changed(
        &mut self,
        _server_id: Option<usize>,
        msg: ChannelEndpointChangedOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {:?}", msg);
        todo!()
    }

    async fn handle_reconnect(
        &mut self,
        _server_id: Option<usize>,
        msg: ReconnectOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {:?}", msg);
        todo!()
    }
}
