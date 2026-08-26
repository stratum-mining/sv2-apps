use crate::{
    downstream::Downstream,
    error::{self, PoolError, PoolErrorKind},
};
use std::{convert::TryInto, sync::atomic::Ordering};
use stratum_apps::{
    stratum_core::{
        common_messages_sv2::{
            ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
            ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, Protocol, SetupConnectionErrorOwned,
            SetupConnectionOwned, SetupConnectionSuccess, has_requires_std_job, has_work_selection,
        },
        handlers_sv2::HandleCommonMessagesFromClientOwnedAsync,
        parsers_sv2::{AnyMessageOwned, Tlv},
    },
    utils::types::{SUPPORTED_PROTOCOL_VERSION, Sv2Frame},
};
use tracing::info;

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleCommonMessagesFromClientOwnedAsync for Downstream {
    type Error = PoolError<error::Downstream>;

    fn get_negotiated_extensions_with_client(
        &self,
        _client_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        self.negotiated_extensions
            .get()
            .map_err(PoolError::shutdown)
    }

    async fn handle_setup_connection(
        &mut self,
        client_id: Option<usize>,
        msg: SetupConnectionOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!(
            "Received `SetupConnection`: min_version={}, max_version={}, flags={:b}",
            msg.min_version, msg.max_version, msg.flags
        );

        let downstream_id = client_id.expect("downstream id should be present");

        if msg.protocol != Protocol::MiningProtocol {
            info!(
                "Rejecting connection from {downstream_id}: SetupConnection asking for other protocols than mining protocol."
            );
            let response = SetupConnectionErrorOwned {
                flags: 0,
                error_code: ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };
            let frame: Sv2Frame = AnyMessageOwned::Common(response.into())
                .try_into()
                .map_err(PoolError::shutdown)?;
            self.downstream_io
                .downstream_sender
                .send(frame.into())
                .await
                .map_err(|_| {
                    PoolError::disconnect(PoolErrorKind::ChannelErrorSender, downstream_id)
                })?;

            return Err(PoolError::disconnect(
                PoolErrorKind::UnsupportedProtocol,
                downstream_id,
            ));
        }

        if SUPPORTED_PROTOCOL_VERSION < msg.min_version
            || SUPPORTED_PROTOCOL_VERSION > msg.max_version
        {
            info!(
                "Rejecting connection from {downstream_id}: no supported protocol version in requested range [{}, {}] (supported: {SUPPORTED_PROTOCOL_VERSION}).",
                msg.min_version, msg.max_version
            );
            let response = SetupConnectionErrorOwned {
                flags: 0,
                error_code: ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };
            let frame: Sv2Frame = AnyMessageOwned::Common(response.into())
                .try_into()
                .map_err(PoolError::shutdown)?;
            self.downstream_io
                .downstream_sender
                .send(frame.into())
                .await
                .map_err(|_| {
                    PoolError::disconnect(PoolErrorKind::ChannelErrorSender, downstream_id)
                })?;

            return Err(PoolError::disconnect(
                PoolErrorKind::SetupConnectionError,
                downstream_id,
            ));
        }

        self.requires_custom_work
            .store(has_work_selection(msg.flags), Ordering::SeqCst);
        self.requires_standard_jobs
            .store(has_requires_std_job(msg.flags), Ordering::SeqCst);

        // SetupConnection.Success.flags for Mining Protocol (server -> client):
        // Bit 0: REQUIRES_FIXED_VERSION (upstream won't accept version changes)
        // Bit 1: REQUIRES_EXTENDED_CHANNELS (upstream won't accept standard channels)
        //
        // When the downstream requests work selection, the pool requires extended channels,
        // since custom work (job declaration) uses extended channels.
        //
        // TODO: replace magic numbers with named constants once
        // https://github.com/stratum-mining/stratum/issues/2075 is resolved.
        let mut response_flags: u32 = 0;
        if has_work_selection(msg.flags) {
            response_flags |= 0x02; // REQUIRES_EXTENDED_CHANNELS
        }
        let response = SetupConnectionSuccess {
            used_version: SUPPORTED_PROTOCOL_VERSION,
            flags: response_flags,
        };
        let frame: Sv2Frame = AnyMessageOwned::Common(response.into())
            .try_into()
            .map_err(PoolError::shutdown)?;
        self.downstream_io
            .downstream_sender
            .send(frame.into())
            .await
            .map_err(|_| {
                PoolError::disconnect(PoolErrorKind::ChannelErrorSender, self.downstream_id)
            })?;

        Ok(())
    }
}
