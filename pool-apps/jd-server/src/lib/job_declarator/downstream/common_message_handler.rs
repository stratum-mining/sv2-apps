use crate::{
    error::{self, JDSError, JDSErrorKind},
    job_declarator::downstream::Downstream,
};
use std::convert::TryInto;
use stratum_apps::{
    stratum_core::{
        common_messages_sv2::{
            Protocol, SetupConnection, SetupConnectionError, SetupConnectionSuccess,
        },
        handlers_sv2::HandleCommonMessagesFromClientAsync,
        parsers_sv2::{AnyMessage, Tlv},
    },
    utils::types::Sv2Frame,
};
use tracing::info;

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleCommonMessagesFromClientAsync for Downstream {
    type Error = JDSError<error::Downstream>;

    fn get_negotiated_extensions_with_client(
        &self,
        _client_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        Ok(self
            .negotiated_extensions
            .super_safe_lock(|extensions| extensions.clone()))
    }

    async fn handle_setup_connection(
        &mut self,
        client_id: Option<usize>,
        msg: SetupConnection<'_>,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!(
            "Received `SetupConnection`: version={}, flags={:b}",
            msg.min_version, msg.flags
        );

        let downstream_id = client_id.expect("downstream id should be present");

        if msg.protocol != Protocol::JobDeclarationProtocol {
            info!("Rejecting connection from {downstream_id}: SetupConnection asking for other protocols than mining protocol.");
            let response = SetupConnectionError {
                flags: 0,
                error_code: "unsupported-protocol"
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };
            let frame: Sv2Frame = AnyMessage::Common(response.into_static().into())
                .try_into()
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

        // todo: add this to `common_messages_sv2`
        // see https://github.com/stratum-mining/stratum/issues/2117
        let has_declare_tx_data = {
            let flags = msg.flags.reverse_bits();
            let flag = flags >> 31;
            flag != 0
        };

        if !has_declare_tx_data {
            info!("Rejecting connection from {downstream_id}: SetupConnection missing DECLARE_TX_DATA flag.");
            let response = SetupConnectionError {
                flags: 0,
                error_code: "missing-declare-tx-data-flag"
                    .to_string()
                    .try_into()
                    .expect("error code must be valid string"),
            };
            let frame: Sv2Frame = AnyMessage::Common(response.into_static().into())
                .try_into()
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
            used_version: 2,
            flags: 0,
        };
        let frame: Sv2Frame = AnyMessage::Common(response.into_static().into())
            .try_into()
            .map_err(JDSError::shutdown)?;
        self.downstream_io
            .to_downstream_sender
            .send(frame)
            .await
            .map_err(|e| JDSError::disconnect(e, self.downstream_id))?;

        Ok(())
    }
}
