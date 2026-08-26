use crate::{
    downstream::Downstream,
    error::{self, JDCError, JDCErrorKind},
};
use std::{convert::TryInto, sync::atomic::Ordering};
#[cfg(feature = "monitoring")]
use stratum_apps::monitoring::client::Sv2ClientKind;
use stratum_apps::{
    stratum_core::{
        common_messages_sv2::{
            ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
            ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS,
            ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, Protocol, SetupConnectionErrorOwned,
            SetupConnectionOwned, SetupConnectionSuccess, has_requires_std_job, has_work_selection,
        },
        handlers_sv2::HandleCommonMessagesFromClientOwnedAsync,
        parsers_sv2::{AnyMessageOwned, Tlv},
    },
    utils::types::{SUPPORTED_PROTOCOL_VERSION, Sv2Frame},
};
use tracing::{error, info};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleCommonMessagesFromClientOwnedAsync for Downstream {
    type Error = JDCError<error::Downstream>;

    fn get_negotiated_extensions_with_client(
        &self,
        _client_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        self.negotiated_extensions.get().map_err(JDCError::shutdown)
    }
    // Handles the initial [`SetupConnection`] message from a downstream client.
    //
    // This method validates that the connection request is compatible with the
    // supported mining protocol and feature set. The flow is:
    //
    // 1. Protocol validation
    //    - Only the `MiningProtocol` is supported.
    //    - If the client requests another protocol, the connection is rejected with a
    //      [`SetupConnectionError`] (`unsupported-protocol`).
    //
    // 2. Protocol version validation
    //    - The supported version (2) must fall within the client's requested `[min_version,
    //      max_version]` range.
    //    - Otherwise, the connection is rejected with a [`SetupConnectionError`]
    //      (`protocol-version-mismatch`).
    //
    // 3. Feature flag validation
    //    - Work selection (`work_selection`) is not allowed.
    //    - If requested, the connection is rejected with a [`SetupConnectionError`]
    //      (`unsupported-feature-flags`).
    //
    // 4. Standard job requirement
    //    - If the downstream sets the `requires_standard_job` flag, it is recorded in
    //      [`Downstream::require_std_job`].
    //
    // 5. Successful setup
    //    - If all validations pass, a [`SetupConnectionSuccess`] message is
    async fn handle_setup_connection(
        &mut self,
        _client_id: Option<usize>,
        msg: SetupConnectionOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {}", msg);

        if msg.protocol != Protocol::MiningProtocol {
            info!(
                "Rejecting connection: SetupConnection asking for other protocols than mining protocol."
            );
            let response = SetupConnectionErrorOwned {
                flags: 0,
                error_code: ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL
                    .to_string()
                    .try_into()
                    .map_err(JDCError::shutdown)?,
            };
            let frame: Sv2Frame = AnyMessageOwned::Common(response.into())
                .try_into()
                .map_err(JDCError::shutdown)?;
            if let Err(e) = self
                .downstream_io
                .downstream_sender
                .send(frame.into())
                .await
            {
                error!(
                    "Failed to send SetupConnectionError to downstream {}: {e}",
                    self.downstream_id
                );
            }

            return Err(JDCError::disconnect(
                JDCErrorKind::SetupConnectionError,
                self.downstream_id,
            ));
        }

        if SUPPORTED_PROTOCOL_VERSION < msg.min_version
            || SUPPORTED_PROTOCOL_VERSION > msg.max_version
        {
            info!(
                "Rejecting connection: no supported protocol version in requested range [{}, {}] (supported: {SUPPORTED_PROTOCOL_VERSION}).",
                msg.min_version, msg.max_version
            );
            let response = SetupConnectionErrorOwned {
                flags: 0,
                error_code: ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH
                    .to_string()
                    .try_into()
                    .map_err(JDCError::shutdown)?,
            };
            let frame: Sv2Frame = AnyMessageOwned::Common(response.into())
                .try_into()
                .map_err(JDCError::shutdown)?;
            if let Err(e) = self
                .downstream_io
                .downstream_sender
                .send(frame.into())
                .await
            {
                error!(
                    "Failed to send SetupConnectionError to downstream {}: {e}",
                    self.downstream_id
                );
            }

            return Err(JDCError::disconnect(
                JDCErrorKind::SetupConnectionError,
                self.downstream_id,
            ));
        }

        if has_work_selection(msg.flags) {
            info!("Rejecting: work selection not allowed.");
            let response = SetupConnectionErrorOwned {
                flags: 0b0000_0000_0000_0010,
                error_code: ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS
                    .to_string()
                    .try_into()
                    .map_err(JDCError::shutdown)?,
            };
            let frame: Sv2Frame = AnyMessageOwned::Common(response.into())
                .try_into()
                .map_err(JDCError::shutdown)?;
            if let Err(e) = self
                .downstream_io
                .downstream_sender
                .send(frame.into())
                .await
            {
                error!(
                    "Failed to send SetupConnectionError to downstream {}: {e}",
                    self.downstream_id
                );
            }

            return Err(JDCError::disconnect(
                JDCErrorKind::SetupConnectionError,
                self.downstream_id,
            ));
        }

        if has_requires_std_job(msg.flags) {
            self.require_std_job.store(true, Ordering::Relaxed);
        }

        #[cfg(feature = "monitoring")]
        {
            let vendor = msg.vendor.as_utf8_or_hex();
            let hardware_version = msg.hardware_version.as_utf8_or_hex();
            self.client_kind
                .set(Sv2ClientKind::from_setup_connection(
                    &vendor,
                    &hardware_version,
                ))
                .map_err(JDCError::shutdown)?;
        }

        let response = SetupConnectionSuccess {
            used_version: SUPPORTED_PROTOCOL_VERSION,
            flags: 0, // !REQUIRES_FIXED_VERSION, !REQUIRES_EXTENDED_CHANNELS
        };
        let frame: Sv2Frame = AnyMessageOwned::Common(response.into())
            .try_into()
            .map_err(JDCError::shutdown)?;

        if let Err(e) = self
            .downstream_io
            .downstream_sender
            .send(frame.into())
            .await
        {
            error!(
                "Failed to send SetupConnectionSuccess to downstream {}: {e}",
                self.downstream_id
            );
            return Err(JDCError::disconnect(
                JDCErrorKind::ChannelErrorSender,
                self.downstream_id,
            ));
        }

        Ok(())
    }
}
