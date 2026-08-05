use stratum_apps::stratum_core::sv1_api::{
    IsServer, client_to_server, json_rpc,
    server_to_client::{self, Notify},
    utils::{Extranonce, HexU32Be, VERSION_ROLLING_MASK},
};
use tracing::{debug, error, info, warn};

use crate::{
    error::TproxyErrorKind,
    sv1::{Downstream, Sv1Server},
    utils::{
        AGGREGATED_CHANNEL_ID, SubmitShareWithChannelId, sv1_worker_name_from_sv1_username,
        validate_sv1_share,
    },
};

impl Sv1Server {
    fn with_registered_downstream_sv1<R, F>(
        &self,
        downstream_id: usize,
        f: F,
    ) -> Result<R, TproxyErrorKind>
    where
        F: FnOnce(&Downstream) -> Result<R, TproxyErrorKind>,
    {
        match self
            .downstreams
            .with(&downstream_id, |downstream| f(downstream))
        {
            Some(result) => result,
            None => Err(TproxyErrorKind::DownstreamNotPresent(downstream_id)),
        }
    }
}

// Implements `IsServer` for `Sv1Server` to handle the Sv1 messages.
#[cfg_attr(not(test), hotpath::measure_all)]
impl IsServer for Sv1Server {
    type Error = TproxyErrorKind;

    fn handle_configure(
        &mut self,
        client_id: Option<usize>,
        request: &client_to_server::Configure,
    ) -> Result<(Option<server_to_client::VersionRollingParams>, Option<bool>), Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");

        info!("Received mining.configure from SV1 downstream");
        debug!("Downstream {downstream_id}: mining.configure = {}", request);

        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| {
                    data.version_rolling_mask = request
                        .version_rolling_mask()
                        .map(|mask| HexU32Be(mask & VERSION_ROLLING_MASK));

                    data.version_rolling_min_bit = request.version_rolling_min_bit_count();

                    debug!(
                        "Negotiated version_rolling_mask: {:?}",
                        data.version_rolling_mask
                    );

                    let params = server_to_client::VersionRollingParams::new(
                        data.version_rolling_mask.clone().unwrap_or(HexU32Be(0)),
                        data.version_rolling_min_bit.clone().unwrap_or(HexU32Be(0)),
                    );

                    (Some(params), Some(false))
                })
                .map_err(Into::into)
        })
    }

    fn handle_subscribe(
        &self,
        client_id: Option<usize>,
        request: &client_to_server::Subscribe,
    ) -> Result<Vec<(String, String)>, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");

        info!("Received mining.subscribe from Sv1 downstream");
        debug!("Down: Handling mining.subscribe: {}", request);

        let set_difficulty_sub = (
            "mining.set_difficulty".to_string(),
            downstream_id.to_string(),
        );

        let notify_sub = (
            "mining.notify".to_string(),
            "ae6812eb4cd7735a302a8a9dd95cf71f".to_string(),
        );

        Ok(vec![set_difficulty_sub, notify_sub])
    }

    fn handle_authorize(
        &self,
        client_id: Option<usize>,
        request: &client_to_server::Authorize,
    ) -> Result<bool, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        info!("Received mining.authorize from Sv1 downstream {downstream_id}");
        debug!("Down: Handling mining.authorize: {}", request);
        Ok(true)
    }

    fn handle_submit(
        &self,
        client_id: Option<usize>,
        request: &client_to_server::Submit,
    ) -> Result<bool, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");

        let job_id = &request.job_id;

        let Some(channel_id) =
            self.with_registered_downstream_sv1(downstream_id, |downstream| {
                downstream
                    .downstream_data
                    .with(|data| data.channel_id)
                    .map_err(Into::into)
            })?
        else {
            return Ok(false);
        };

        let channel_id = if self.mode.is_aggregated() {
            AGGREGATED_CHANNEL_ID
        } else {
            channel_id
        };

        let find_job = |jobs: &[Notify]| jobs.iter().find(|j| j.job_id == *job_id).cloned();

        let job = self
            .valid_sv1_jobs
            .with(&channel_id, |jobs| find_job(jobs.as_ref()))
            .flatten();

        let Some(job) = job else {
            return Ok(false);
        };

        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| {
                    let channel_id = match data.channel_id {
                        Some(id) => id,
                        None => {
                            error!(
                                "Cannot submit share: channel_id is None \
                         (waiting for OpenExtendedMiningChannelSuccess)"
                            );
                            return Ok(false);
                        }
                    };

                    info!(
                        "Received mining.submit from SV1 downstream for channel id: {}",
                        channel_id
                    );

                    let is_valid = validate_sv1_share(
                        request,
                        data.target,
                        data.extranonce1.clone().into(),
                        data.version_rolling_mask.clone(),
                        job.clone(),
                    )
                    .unwrap_or(false);

                    if !is_valid {
                        error!("Invalid share for channel id: {}", channel_id);
                        return Ok(false);
                    }

                    data.pending_share = Some(SubmitShareWithChannelId {
                        channel_id,
                        downstream_id,
                        share: request.clone(),
                        extranonce: data.extranonce1.clone().into(),
                        extranonce2_len: data.extranonce2_len,
                        version_rolling_mask: data.version_rolling_mask.clone(),
                        job_version: data.last_job_version_field,
                    });

                    Ok(true)
                })
                .map_err(TproxyErrorKind::from)?
        })
    }

    /// Indicates to the server that the client supports the mining.set_extranonce method.
    fn handle_extranonce_subscribe(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Checks if a Downstream role is authorized.
    fn is_authorized(&self, client_id: Option<usize>, name: &str) -> Result<bool, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        let is_authorized = self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.sv1_username == *name)
                .map_err(Into::into)
        })?;
        Ok(is_authorized)
    }

    /// Authorizes a Downstream role.
    fn authorize(&mut self, client_id: Option<usize>, name: &str) -> Result<(), Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        let is_authorized = self.is_authorized(client_id, name)?;
        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| {
                    if !is_authorized {
                        data.sv1_username = name.to_string();
                    }
                    data.sv1_worker_name = sv1_worker_name_from_sv1_username(name).to_string();
                    debug!(
                        "Down: Set sv1_username '{}' and sv1_worker_name '{}' for downstream {}",
                        data.sv1_username, data.sv1_worker_name, downstream_id
                    );
                })
                .map_err(Into::into)
        })?;
        Ok(())
    }

    /// Sets the `extranonce1` field sent in the SV1 `mining.notify` message to the value specified
    /// by the SV2 `OpenExtendedMiningChannelSuccess` message sent from the Upstream role.
    fn set_extranonce1(
        &mut self,
        client_id: Option<usize>,
        _extranonce1: Option<Extranonce>,
    ) -> Result<Extranonce, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.extranonce1.clone())
                .map_err(Into::into)
        })
    }

    /// Returns the `Downstream`'s `extranonce1` value.
    fn extranonce1(&self, client_id: Option<usize>) -> Result<Extranonce, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.extranonce1.clone())
                .map_err(Into::into)
        })
    }

    /// Sets the `extranonce2_size` field sent in the SV1 `mining.notify` message to the value
    /// specified by the SV2 `OpenExtendedMiningChannelSuccess` message sent from the Upstream role.
    fn set_extranonce2_size(
        &mut self,
        client_id: Option<usize>,
        _extra_nonce2_size: Option<usize>,
    ) -> Result<usize, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.extranonce2_len)
                .map_err(Into::into)
        })
    }

    /// Returns the `Downstream`'s `extranonce2_size` value.
    fn extranonce2_size(&self, client_id: Option<usize>) -> Result<usize, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.extranonce2_len)
                .map_err(Into::into)
        })
    }

    /// Returns the version rolling mask.
    fn version_rolling_mask(
        &self,
        client_id: Option<usize>,
    ) -> Result<Option<HexU32Be>, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.version_rolling_mask.clone())
                .map_err(Into::into)
        })
    }

    /// Sets the version rolling mask.
    fn set_version_rolling_mask(
        &mut self,
        client_id: Option<usize>,
        mask: Option<HexU32Be>,
    ) -> Result<(), Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.version_rolling_mask = mask)
                .map_err(Into::into)
        })?;

        Ok(())
    }

    /// Sets the minimum version rolling bit.
    fn set_version_rolling_min_bit(
        &mut self,
        client_id: Option<usize>,
        mask: Option<HexU32Be>,
    ) -> Result<(), Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream_sv1(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.version_rolling_min_bit = mask)
                .map_err(Into::into)
        })?;
        Ok(())
    }

    fn notify(&mut self, _client_id: Option<usize>) -> Result<json_rpc::Message, Self::Error> {
        warn!("notify() called on Sv1Server - this method is not implemented for Sv1Server");
        Err(
            stratum_apps::stratum_core::sv1_api::error::Error::UnexpectedMessage(
                "notify".to_string(),
            )
            .into(),
        )
    }
}
