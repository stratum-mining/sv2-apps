use stratum_apps::stratum_core::sv1_api::{
    Authorize, Configure, ConfigureResult, IsServer, Submit, SubmitError, SubmitOutcome, Subscribe,
    SubscribeResult, SuggestDifficulty, VersionRollingResult,
    utils::{HexU32Be, VERSION_ROLLING_MASK},
};
use tracing::{debug, error, info};

use crate::{
    error::{self, TproxyError},
    sv1::Sv1Server,
    utils::{
        AGGREGATED_CHANNEL_ID, SubmitShareWithChannelId, Sv1ShareValidationOutcome,
        sv1_worker_name_from_sv1_username, validate_sv1_share,
    },
};

// Implements `IsServer` for `Sv1Server` to handle the Sv1 messages.
#[cfg_attr(not(test), hotpath::measure_all)]
impl IsServer for Sv1Server {
    type Error = TproxyError<error::Sv1Server>;

    fn handle_configure(
        &mut self,
        client_id: Option<usize>,
        request: &Configure,
    ) -> Result<ConfigureResult, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");

        info!("Received mining.configure from SV1 downstream");
        debug!(
            "Downstream {downstream_id}: mining.configure = {:?}",
            request
        );

        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| {
                    data.version_rolling_mask = request
                        .version_rolling_mask()
                        .map(|mask| HexU32Be(mask & VERSION_ROLLING_MASK));

                    data.version_rolling_min_bit =
                        request.version_rolling_min_bit_count().map(HexU32Be);

                    debug!(
                        "Negotiated version_rolling_mask: {:?}",
                        data.version_rolling_mask
                    );

                    ConfigureResult {
                        version_rolling: Some(VersionRollingResult::new(
                            data.version_rolling_mask.clone().unwrap_or(HexU32Be(0)),
                            data.version_rolling_min_bit
                                .clone()
                                .map_or(0, |count| count.0),
                        )),
                        minimum_difficulty: Some(false),
                    }
                })
                .map_err(TproxyError::shutdown)
        })
    }

    /// Returns the `extranonce1` and `extranonce2_size` assigned by the SV2
    /// `OpenExtendedMiningChannelSuccess` message from the Upstream role.
    fn handle_subscribe(
        &mut self,
        client_id: Option<usize>,
        request: &Subscribe,
    ) -> Result<SubscribeResult, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");

        info!("Received mining.subscribe from Sv1 downstream");
        debug!("Down: Handling mining.subscribe: {:?}", request);

        let set_difficulty_sub = (
            "mining.set_difficulty".to_string(),
            downstream_id.to_string(),
        );

        let notify_sub = (
            "mining.notify".to_string(),
            "ae6812eb4cd7735a302a8a9dd95cf71f".to_string(),
        );

        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| SubscribeResult {
                    subscriptions: vec![set_difficulty_sub, notify_sub],
                    extranonce1: data.extranonce1.clone(),
                    extranonce2_size: data.extranonce2_len,
                })
                .map_err(TproxyError::shutdown)
        })
    }

    /// Authorizes every worker and records its user name.
    fn handle_authorize(
        &mut self,
        client_id: Option<usize>,
        request: &Authorize,
    ) -> Result<bool, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        info!("Received mining.authorize from Sv1 downstream {downstream_id}");
        debug!("Down: Handling mining.authorize: {:?}", request);

        let name = &request.worker_name;
        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| {
                    data.sv1_username = name.to_string();
                    data.sv1_worker_name = sv1_worker_name_from_sv1_username(name).to_string();
                    debug!(
                        "Down: Set sv1_username '{}' and sv1_worker_name '{}' for downstream {}",
                        data.sv1_username, data.sv1_worker_name, downstream_id
                    );
                })
                .map_err(TproxyError::shutdown)
        })?;
        Ok(true)
    }

    fn handle_submit(
        &mut self,
        client_id: Option<usize>,
        request: &Submit,
    ) -> Result<SubmitOutcome, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");

        let job_id = &request.job_id;

        let Some(channel_id) = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.channel_id)
                .map_err(TproxyError::shutdown)
        })?
        else {
            return Ok(SubmitOutcome::Rejected(SubmitError::Other));
        };

        let channel_id = if self.mode.is_aggregated() {
            AGGREGATED_CHANNEL_ID
        } else {
            channel_id
        };

        let job = self
            .channels
            .with(&channel_id, |channel| {
                channel
                    .jobs
                    .as_ref()
                    .and_then(|jobs| jobs.get(job_id).map(|job| job.notify.clone()))
            })
            .flatten();

        let Some(job) = job else {
            return Ok(SubmitOutcome::Rejected(SubmitError::JobNotFound));
        };

        self.with_registered_downstream(downstream_id, |downstream| {
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
                            return Ok(SubmitOutcome::Rejected(SubmitError::Other));
                        }
                    };

                    info!(
                        "Received mining.submit from SV1 downstream for channel id: {}",
                        channel_id
                    );

                    let Some(job_context) = data.job_validation_context(job_id) else {
                        debug!(
                            job_id,
                            channel_id, "Submitted SV1 job is no longer valid for this downstream"
                        );
                        return Ok(SubmitOutcome::Rejected(SubmitError::JobNotFound));
                    };
                    let share_hash = match validate_sv1_share(
                        request,
                        job_context.target,
                        job_context.extranonce.clone().into(),
                        data.version_rolling_mask.clone(),
                        job.clone(),
                    ) {
                        Ok(Sv1ShareValidationOutcome::MeetsTarget(hash)) => hash,
                        Ok(Sv1ShareValidationOutcome::DoesNotMeetTarget) => {
                            return Ok(SubmitOutcome::Rejected(SubmitError::LowDifficultyShare));
                        }
                        Err(error) => {
                            debug!(channel_id, ?error, "SV1 share could not be validated");
                            return Ok(SubmitOutcome::Rejected(SubmitError::Other));
                        }
                    };

                    if !data.accepted_share_hashes.insert_if_new(share_hash) {
                        return Ok(SubmitOutcome::Rejected(SubmitError::DuplicateShare));
                    }

                    data.pending_share = Some(SubmitShareWithChannelId {
                        channel_id,
                        downstream_id,
                        share: request.clone(),
                        extranonce: job_context.extranonce.into(),
                        extranonce2_len: job_context.extranonce2_len,
                        version_rolling_mask: data.version_rolling_mask.clone(),
                        job_version: Some(job.version.0),
                    });

                    Ok(SubmitOutcome::Accepted)
                })
                .map_err(TproxyError::shutdown)?
        })
    }

    /// Indicates to the server that the client supports the mining.set_extranonce method.
    fn handle_extranonce_subscribe(&mut self, client_id: Option<usize>) -> Result<(), Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.supports_set_extranonce = true)
                .map_err(TproxyError::shutdown)
        })
    }

    /// The suggestion is not used: the initial target comes from the configured hashrate, then
    /// vardiff.
    fn handle_suggest_difficulty(
        &mut self,
        _client_id: Option<usize>,
        _request: &SuggestDifficulty,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Checks if a Downstream role is authorized.
    fn is_authorized(&self, client_id: Option<usize>, name: &str) -> Result<bool, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        let is_authorized = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.sv1_username == *name)
                .map_err(TproxyError::shutdown)
        })?;
        Ok(is_authorized)
    }

    /// Returns the `Downstream`'s `extranonce2_size` value.
    fn extranonce2_size(&self, client_id: Option<usize>) -> Result<usize, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.extranonce2_len)
                .map_err(TproxyError::shutdown)
        })
    }

    /// Returns the version rolling mask.
    fn version_rolling_mask(
        &self,
        client_id: Option<usize>,
    ) -> Result<Option<HexU32Be>, Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.version_rolling_mask.clone())
                .map_err(TproxyError::shutdown)
        })
    }
}
