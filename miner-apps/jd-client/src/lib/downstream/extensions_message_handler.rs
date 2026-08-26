use crate::{
    downstream::Downstream,
    error::{self, JDCError, JDCErrorKind},
};
use stratum_apps::{
    stratum_core::{
        binary_sv2::Seq064KOwned,
        extensions_sv2::{
            RequestExtensionsErrorOwned, RequestExtensionsOwned, RequestExtensionsSuccessOwned,
        },
        handlers_sv2::HandleExtensionsFromClientOwnedAsync,
        parsers_sv2::{AnyMessageOwned, Tlv},
    },
    utils::types::Sv2Frame,
};
use tracing::{error, info};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleExtensionsFromClientOwnedAsync for Downstream {
    type Error = JDCError<error::Downstream>;

    fn get_negotiated_extensions_with_client(
        &self,
        _client_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        self.negotiated_extensions.get().map_err(JDCError::shutdown)
    }

    async fn handle_request_extensions(
        &mut self,
        _client_id: Option<usize>,
        msg: RequestExtensionsOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        let requested: Vec<u16> = msg.requested_extensions.clone().into_inner();

        info!(
            "Downstream {}: Received RequestExtensions: request_id={}, requested={:?}",
            self.downstream_id, msg.request_id, requested
        );

        // Determine which requested extensions we support
        let mut supported: Vec<u16> = Vec::new();
        let mut unsupported: Vec<u16> = Vec::new();

        for ext in &requested {
            if self.supported_extensions.contains(ext) {
                supported.push(*ext);
            } else {
                unsupported.push(*ext);
            }
        }

        // Check which required extensions the client didn't request
        let missing_required: Vec<u16> = self
            .required_extensions
            .iter()
            .filter(|ext| !requested.contains(ext))
            .copied()
            .collect();

        // Determine response based on spec rules:
        // - Success: If at least one extension is supported AND all required extensions are present
        // - Error: If no extensions are supported OR required extensions are missing
        let should_send_error = supported.is_empty() || !missing_required.is_empty();

        if should_send_error {
            // Send error response
            error!(
                "Downstream {}: Extension negotiation error: requested={:?}, supported={:?}, unsupported={:?}, missing_required={:?}",
                self.downstream_id, requested, supported, unsupported, missing_required
            );

            let error = RequestExtensionsErrorOwned {
                request_id: msg.request_id,
                unsupported_extensions: Seq064KOwned::new(unsupported).map_err(|_| {
                    JDCError::shutdown(JDCErrorKind::InvalidUnsupportedExtensionsSequence)
                })?,
                required_extensions: Seq064KOwned::new(missing_required.clone()).map_err(|_| {
                    JDCError::shutdown(JDCErrorKind::InvalidRequiredExtensionsSequence)
                })?,
            };

            let frame: Sv2Frame = AnyMessageOwned::Extensions(error.into())
                .try_into()
                .map_err(JDCError::shutdown)?;
            if let Err(e) = self
                .downstream_io
                .downstream_sender
                .send(frame.into())
                .await
            {
                error!(
                    "Failed to send RequestExtensionsError to downstream {}: {e}",
                    self.downstream_id
                );
                return Err(JDCError::disconnect(
                    JDCErrorKind::ChannelErrorSender,
                    self.downstream_id,
                ));
            }

            // If required extensions are missing, the server SHOULD disconnect the client
            if !missing_required.is_empty() {
                error!(
                    "Downstream {}: Client does not support required extensions {:?}. Server MUST disconnect.",
                    self.downstream_id, missing_required
                );
                // TODO: Disconnect the client
            }
        } else {
            // Send success response with the subset of extensions we both support
            info!(
                "Downstream {}: Extension negotiation success: requested={:?}, negotiated={:?}",
                self.downstream_id, requested, supported
            );

            // Store the negotiated extensions
            self.negotiated_extensions
                .set(supported.clone())
                .map_err(JDCError::shutdown)?;

            let success = RequestExtensionsSuccessOwned {
                request_id: msg.request_id,
                supported_extensions: Seq064KOwned::new(supported.clone()).map_err(|_| {
                    JDCError::shutdown(JDCErrorKind::InvalidSupportedExtensionsSequence)
                })?,
            };

            let frame: Sv2Frame = AnyMessageOwned::Extensions(success.into())
                .try_into()
                .map_err(JDCError::shutdown)?;
            if let Err(e) = self
                .downstream_io
                .downstream_sender
                .send(frame.into())
                .await
            {
                error!(
                    "Failed to send RequestExtensionsSuccess to downstream {}: {e}",
                    self.downstream_id
                );
                return Err(JDCError::disconnect(
                    JDCErrorKind::ChannelErrorSender,
                    self.downstream_id,
                ));
            }

            info!(
                "Downstream {}: Stored negotiated extensions: {:?}",
                self.downstream_id, supported
            );
        }

        Ok(())
    }
}
