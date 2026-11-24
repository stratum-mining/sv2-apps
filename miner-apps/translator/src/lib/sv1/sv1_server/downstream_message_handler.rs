use stratum_apps::{
    stratum_core::sv1_api::{
        Authorize, Configure, ConfigureResult, Id, IsServerAsync, Response, Submit, SubmitError,
        SubmitOutcome, Subscribe, SubscribeResult, SuggestDifficulty, VersionRollingResult,
        utils::VERSION_ROLLING_MASK,
    },
    utils::types::DownstreamId,
};
use tracing::{debug, error, info};

use crate::{
    error::{self, TproxyError, TproxyResult},
    sv1::{Sv1Server, downstream::Sv1SetupRequest},
    utils::{
        AGGREGATED_CHANNEL_ID, SubmitShareWithChannelId, Sv1ShareValidationOutcome,
        sv1_worker_name_from_sv1_username, validate_sv1_share,
    },
};

// Implements `IsServerAsync` for `Sv1Server`: each handler acts on its request and queues the
// reply to the miner.
#[cfg_attr(not(test), hotpath::measure_all)]
impl IsServerAsync for Sv1Server {
    type Error = TproxyError<error::Sv1Server>;

    async fn handle_configure(
        &mut self,
        client_id: Option<usize>,
        id: Id,
        request: Configure,
    ) -> Result<(), Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");

        info!("Received mining.configure from SV1 downstream");
        debug!(
            "Downstream {downstream_id}: mining.configure = {:?}",
            request
        );

        let result = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| {
                    data.version_rolling_mask = request
                        .version_rolling_mask()
                        .map(|mask| mask & VERSION_ROLLING_MASK);

                    data.version_rolling_min_bit = request.version_rolling_min_bit_count();
                    if request.supports_extranonce() {
                        data.supports_set_extranonce = true;
                    }

                    debug!(
                        "Negotiated version_rolling_mask: {:?}",
                        data.version_rolling_mask
                    );

                    // Each requested extension gets a result, and no other key is sent.
                    ConfigureResult {
                        version_rolling: data.version_rolling_mask.map(VersionRollingResult::new),
                        minimum_difficulty: request.minimum_difficulty().map(|_| false),
                        subscribe_extranonce: request.supports_extranonce().then_some(true),
                        info: request.info().map(|_| true),
                    }
                })
                .map_err(TproxyError::shutdown)
        })?;
        self.send_response(downstream_id, Response::success(id, result))
            .await
    }

    /// Returns the `extranonce1` and `extranonce2_size` assigned by the SV2
    /// `OpenExtendedMiningChannelSuccess` message from the Upstream role.
    async fn handle_subscribe(
        &mut self,
        client_id: Option<usize>,
        id: Id,
        request: Subscribe,
    ) -> Result<(), Self::Error> {
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

        let result = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| SubscribeResult {
                    subscriptions: vec![set_difficulty_sub, notify_sub],
                    extranonce1: data.extranonce1.clone(),
                    extranonce2_size: data.extranonce2_len,
                })
                .map_err(TproxyError::shutdown)
        })?;
        self.send_response(downstream_id, Response::success(id, result))
            .await?;
        self.record_setup_response(downstream_id, Sv1SetupRequest::Subscribe)
            .await
    }

    /// Authorizes every worker and records its user name.
    async fn handle_authorize(
        &mut self,
        client_id: Option<usize>,
        id: Id,
        request: Authorize,
    ) -> Result<(), Self::Error> {
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
        self.send_response(downstream_id, Response::success(id, true))
            .await?;
        self.record_setup_response(downstream_id, Sv1SetupRequest::Authorize)
            .await
    }

    /// Replies to the miner, then forwards an accepted share upstream.
    async fn handle_submit(
        &mut self,
        client_id: Option<usize>,
        id: Id,
        request: Submit,
    ) -> Result<(), Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        let outcome = match self.check_submit_session(downstream_id, &request)? {
            Some(error) => Err(error),
            None => self.validate_submit(downstream_id, request)?,
        };
        match outcome {
            Ok(share) => {
                self.send_response(downstream_id, SubmitOutcome::Accepted.into_response(id))
                    .await?;
                self.handle_submit_shares(share).await
            }
            Err(error) => {
                self.send_response(
                    downstream_id,
                    SubmitOutcome::Rejected(error).into_response(id),
                )
                .await
            }
        }
    }

    /// Indicates to the server that the client supports the mining.set_extranonce method.
    async fn handle_extranonce_subscribe(
        &mut self,
        client_id: Option<usize>,
        id: Id,
    ) -> Result<(), Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.supports_set_extranonce = true)
                .map_err(TproxyError::shutdown)
        })?;
        self.send_response(downstream_id, Response::success(id, true))
            .await
    }

    /// Acknowledges the suggestion without using it: the initial target comes from the
    /// configured hashrate, then vardiff.
    async fn handle_suggest_difficulty(
        &mut self,
        client_id: Option<usize>,
        id: Id,
        _request: SuggestDifficulty,
    ) -> Result<(), Self::Error> {
        let downstream_id = client_id.expect("Downstream id should exist");
        self.send_response(downstream_id, Response::success(id, true))
            .await
    }
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv1Server {
    /// The session checks a submit passes before its share is validated: its worker is
    /// authorized, its extranonce2 has the size assigned to the miner, and its version bits match
    /// the negotiated mask, including whether they must be present. Bits outside
    /// [`VERSION_ROLLING_MASK`] are rejected even if the negotiated mask allows them.
    ///
    /// Returns the reason the miner is given for a rejection.
    #[allow(clippy::result_large_err)]
    pub(super) fn check_submit_session(
        &self,
        downstream_id: DownstreamId,
        request: &Submit,
    ) -> TproxyResult<Option<SubmitError>, error::Sv1Server> {
        self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| {
                    let has_valid_version_bits =
                        match (&request.version_bits, data.version_rolling_mask) {
                            (Some(bits), Some(mask)) => {
                                (mask & VERSION_ROLLING_MASK).check_mask(bits)
                            }
                            (Some(_), None) => false,
                            (None, mask) => mask.is_none(),
                        };
                    if data.sv1_username != request.worker_name {
                        Some(SubmitError::UnauthorizedWorker)
                    } else if data.extranonce2_len != request.extranonce2.len()
                        || !has_valid_version_bits
                    {
                        Some(SubmitError::Other)
                    } else {
                        None
                    }
                })
                .map_err(TproxyError::shutdown)
        })
    }

    /// Validates a submit that passed the session checks.
    ///
    /// Returns the share to forward upstream, or the reason the miner is given for its rejection.
    #[allow(clippy::result_large_err)]
    pub(super) fn validate_submit(
        &self,
        downstream_id: DownstreamId,
        request: Submit,
    ) -> TproxyResult<Result<SubmitShareWithChannelId, SubmitError>, error::Sv1Server> {
        let Some(channel_id) = self.with_registered_downstream(downstream_id, |downstream| {
            downstream
                .downstream_data
                .with(|data| data.channel_id)
                .map_err(TproxyError::shutdown)
        })?
        else {
            return Ok(Err(SubmitError::Other));
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
                    .and_then(|jobs| jobs.get(&request.job_id).map(|job| job.notify.clone()))
            })
            .flatten();

        let Some(job) = job else {
            return Ok(Err(SubmitError::JobNotFound));
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
                            return Ok(Err(SubmitError::Other));
                        }
                    };

                    info!(
                        "Received mining.submit from SV1 downstream for channel id: {}",
                        channel_id
                    );

                    let Some(job_context) = data.job_validation_context(&request.job_id) else {
                        debug!(
                            job_id = request.job_id.as_str(),
                            channel_id, "Submitted SV1 job is no longer valid for this downstream"
                        );
                        return Ok(Err(SubmitError::JobNotFound));
                    };
                    let share_hash = match validate_sv1_share(
                        &request,
                        job_context.target,
                        job_context.extranonce.clone().into(),
                        data.version_rolling_mask,
                        job.clone(),
                    ) {
                        Ok(Sv1ShareValidationOutcome::MeetsTarget(hash)) => hash,
                        Ok(Sv1ShareValidationOutcome::DoesNotMeetTarget) => {
                            return Ok(Err(SubmitError::LowDifficultyShare));
                        }
                        Err(error) => {
                            debug!(channel_id, ?error, "SV1 share could not be validated");
                            return Ok(Err(SubmitError::Other));
                        }
                    };

                    if !data.accepted_share_hashes.insert_if_new(share_hash) {
                        return Ok(Err(SubmitError::DuplicateShare));
                    }

                    Ok(Ok(SubmitShareWithChannelId {
                        channel_id,
                        downstream_id,
                        share: request,
                        extranonce: job_context.extranonce.into(),
                        extranonce2_len: job_context.extranonce2_len,
                        version_rolling_mask: data.version_rolling_mask,
                        job_version: Some(job.version.0),
                    }))
                })
                .map_err(TproxyError::shutdown)?
        })
    }
}
