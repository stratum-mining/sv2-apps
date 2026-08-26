use crate::{
    channel_manager::ChannelManager,
    error::{self, JDCError, JDCErrorKind},
};
use stratum_apps::{
    stratum_core::{
        binary_sv2::Seq064KOwned,
        extensions_sv2::{
            RequestExtensionsErrorOwned, RequestExtensionsOwned, RequestExtensionsSuccessOwned,
        },
        handlers_sv2::HandleExtensionsFromServerOwnedAsync,
        parsers_sv2::{AnyMessageOwned, Tlv},
    },
    utils::types::Sv2Frame,
};
use tracing::{error, info};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleExtensionsFromServerOwnedAsync for ChannelManager {
    type Error = JDCError<error::ChannelManager>;

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        self.negotiated_extensions
            .with(|data| data.clone())
            .map_err(JDCError::shutdown)
    }

    async fn handle_request_extensions_success(
        &mut self,
        _server_id: Option<usize>,
        msg: RequestExtensionsSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let supported: Vec<u16> = msg.supported_extensions.into_inner();

        info!("Extension negotiation success: supported={:?}", supported);

        // Check if all of the proxy's required extensions are supported by the server
        let missing_required: Vec<u16> = self
            .required_extensions
            .iter()
            .filter(|ext| !supported.contains(ext))
            .copied()
            .collect();

        if !missing_required.is_empty() {
            error!(
                "Server does not support our required extensions {:?}. Connection should fail over to another upstream.",
                missing_required
            );
            return Err(JDCError::fallback(
                JDCErrorKind::RequiredExtensionsNotSupported(missing_required),
            ));
        }

        // Store the negotiated extensions in the shared channel manager data
        self.negotiated_extensions
            .with(|data| {
                *data = supported.clone();
            })
            .map_err(JDCError::fallback)?;

        info!("Successfully negotiated extensions: {:?}", supported);

        Ok(())
    }

    async fn handle_request_extensions_error(
        &mut self,
        _server_id: Option<usize>,
        msg: RequestExtensionsErrorOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let unsupported: Vec<u16> = msg.unsupported_extensions.into_inner();
        let required_by_server: Vec<u16> = msg.required_extensions.into_inner();

        error!(
            "Extension negotiation error: unsupported={:?}, required_by_server={:?}",
            unsupported, required_by_server
        );

        // Check if any of our required extensions were not supported by the server
        let missing_required: Vec<u16> = self
            .required_extensions
            .iter()
            .filter(|ext| unsupported.contains(&**ext))
            .copied()
            .collect();

        if !missing_required.is_empty() {
            error!(
                "Server does not support our required extensions {:?}. Connection should fail over to another upstream.",
                missing_required
            );
            return Err(JDCError::fallback(
                JDCErrorKind::RequiredExtensionsNotSupported(missing_required),
            ));
        }

        // Check if server requires extensions - if we support them, we should retry with them
        // included
        if !required_by_server.is_empty() {
            // Check which of the server's required extensions we support
            let can_support: Vec<u16> = required_by_server
                .iter()
                .filter(|ext| self.supported_extensions.contains(ext))
                .copied()
                .collect();

            let cannot_support: Vec<u16> = required_by_server
                .iter()
                .filter(|ext| !self.supported_extensions.contains(ext))
                .copied()
                .collect();

            if !cannot_support.is_empty() {
                // Server requires extensions we don't support - must fail over
                error!(
                    "Server requires extensions {:?} that we don't support. Connection should fail over to another upstream.",
                    cannot_support
                );
                return Err(JDCError::fallback(
                    JDCErrorKind::ServerRequiresUnsupportedExtensions(cannot_support),
                ));
            }

            // All required extensions are supported - we should retry with them included
            info!(
                "Server requires extensions {:?} that we support. Proxy should retry RequestExtensions with these included.",
                can_support
            );

            let new_require_extensions = RequestExtensionsOwned {
                request_id: msg.request_id + 1,
                requested_extensions: Seq064KOwned::new(can_support).unwrap(),
            };

            let sv2_frame: Sv2Frame = AnyMessageOwned::Extensions(new_require_extensions.into())
                .try_into()
                .map_err(JDCError::shutdown)?;

            self.channel_manager_io
                .upstream_sender
                .send(sv2_frame.into())
                .await
                .map_err(|e| {
                    error!("Failed to send message to upstream: {:?}", e);
                    JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                })?;
        }

        Ok(())
    }
}
