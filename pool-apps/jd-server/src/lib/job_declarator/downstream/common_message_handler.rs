use crate::{
    error::{self, JDSError, JDSErrorKind},
    job_declarator::downstream::Downstream,
};
use std::convert::TryInto;
use stratum_apps::{
    stratum_core::{
        common_messages_sv2::{
            ERROR_CODE_SETUP_CONNECTION_MISSING_DECLARE_TX_DATA_FLAG,
            ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
            ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, Protocol, SetupConnectionErrorOwned,
            SetupConnectionOwned, SetupConnectionSuccess,
        },
        handlers_sv2::HandleCommonMessagesFromClientOwnedAsync,
        parsers_sv2::{AnyMessageOwned, Tlv},
    },
    utils::types::{OutboundFrame, SUPPORTED_PROTOCOL_VERSION},
};
use tracing::info;

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleCommonMessagesFromClientOwnedAsync for Downstream {
    type Error = JDSError<error::Downstream>;

    fn get_negotiated_extensions_with_client(
        &self,
        _client_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        self.negotiated_extensions.get().map_err(JDSError::shutdown)
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

        if msg.protocol != Protocol::JobDeclarationProtocol {
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
            let frame = OutboundFrame::from_message(AnyMessageOwned::Common(response.into()))
                .map_err(JDSError::shutdown)?;
            self.downstream_io
                .to_downstream_sender
                .send(frame)
                .await
                .map_err(|e| JDSError::disconnect(e, downstream_id))?;

            return Err(JDSError::disconnect(
                JDSErrorKind::UnsupportedProtocol,
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
                    .map_err(JDSError::shutdown)?,
            };
            let frame = OutboundFrame::from_message(AnyMessageOwned::Common(response.into()))
                .map_err(JDSError::shutdown)?;
            self.downstream_io
                .to_downstream_sender
                .send(frame)
                .await
                .map_err(|e| JDSError::disconnect(e, downstream_id))?;

            return Err(JDSError::disconnect(
                JDSErrorKind::ProtocolVersionMismatch,
                downstream_id,
            ));
        }

        // todo: add this to `common_messages_sv2`
        // see https://github.com/stratum-mining/stratum/issues/2117
        let has_declare_tx_data = {
            let flags = msg.flags.reverse_bits();
            let flag = flags >> 31;
            flag != 0
        };

        if !has_declare_tx_data {
            info!(
                "Rejecting connection from {downstream_id}: SetupConnection missing DECLARE_TX_DATA flag."
            );
            let response = SetupConnectionErrorOwned {
                flags: 0,
                error_code: ERROR_CODE_SETUP_CONNECTION_MISSING_DECLARE_TX_DATA_FLAG
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };
            let frame = OutboundFrame::from_message(AnyMessageOwned::Common(response.into()))
                .map_err(JDSError::shutdown)?;
            self.downstream_io
                .to_downstream_sender
                .send(frame)
                .await
                .map_err(|e| JDSError::disconnect(e, self.downstream_id))?;

            return Err(JDSError::disconnect(
                JDSErrorKind::UnsupportedConnectionFlags,
                downstream_id,
            ));
        }

        let response = SetupConnectionSuccess {
            used_version: SUPPORTED_PROTOCOL_VERSION,
            flags: 0,
        };
        let frame = OutboundFrame::from_message(AnyMessageOwned::Common(response.into()))
            .map_err(JDSError::shutdown)?;
        self.downstream_io
            .to_downstream_sender
            .send(frame)
            .await
            .map_err(|e| JDSError::disconnect(e, self.downstream_id))?;

        Ok(())
    }
}
