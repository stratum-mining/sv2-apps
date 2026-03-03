use std::sync::{
    atomic::{AtomicU32, AtomicUsize, Ordering},
    Arc,
};

use async_channel::Sender;
use stratum_apps::stratum_core::{
    job_declaration_sv2::{AllocateMiningJobToken, AllocateMiningJobTokenSuccess},
    parsers_sv2::JobDeclaration,
};
use tracing::{debug, info};

use crate::error::{self, JDCError, JDCErrorKind};

pub const TOKEN_QUEUE_TARGET_SIZE: usize = 4;
pub const TOKEN_REFILL_THRESHOLD: usize = 2;

/// Owns the mining-job token queue for the JDC.
#[derive(Clone)]
pub struct TokenManager {
    token_tx: async_channel::Sender<AllocateMiningJobTokenSuccess<'static>>,
    token_rx: async_channel::Receiver<AllocateMiningJobTokenSuccess<'static>>,
    pending_count: Arc<AtomicUsize>,
    jd_sender: Sender<JobDeclaration<'static>>,
    user_identity: String,
    request_id_factory: Arc<AtomicU32>,
}

impl TokenManager {
    pub fn new(
        jd_sender: Sender<JobDeclaration<'static>>,
        user_identity: String,
        request_id_factory: Arc<AtomicU32>,
    ) -> Self {
        let (token_tx, token_rx) = async_channel::unbounded();
        Self {
            token_tx,
            token_rx,
            pending_count: Arc::new(AtomicUsize::new(0)),
            jd_sender,
            user_identity,
            request_id_factory,
        }
    }

    pub async fn init(&self) -> Result<(), JDCError<error::ChannelManager>> {
        self.request_tokens(TOKEN_QUEUE_TARGET_SIZE).await
    }

    pub fn drain(&self) {
        while self.token_rx.try_recv().is_ok() {}
        self.pending_count.store(0, Ordering::Relaxed);
    }

    pub fn push(&self, token: AllocateMiningJobTokenSuccess<'static>) {
        self.pending_count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            })
            .ok();

        // here, the channel is used for its buffer and very nice
        // push and pull semantics. We can ignore this error, as
        // it should only happen in cases when the receiver end
        // is no longer there but considering both sender and
        // receiver are always there, so it will never error
        // out.
        if let Err(e) = self.token_tx.try_send(token) {
            tracing::error!("Token queue closed unexpectedly: {e}");
        }
    }

    pub async fn take(
        &self,
    ) -> Result<AllocateMiningJobTokenSuccess<'static>, JDCError<error::ChannelManager>> {
        self.schedule_refill_if_needed().await;
        self.token_rx
            .recv()
            .await
            .map_err(|_| JDCError::log(JDCErrorKind::TokenNotFound))
    }

    async fn schedule_refill_if_needed(&self) {
        let effective = self.token_rx.len() + self.pending_count.load(Ordering::Relaxed);
        if effective < TOKEN_REFILL_THRESHOLD {
            let needed = TOKEN_QUEUE_TARGET_SIZE.saturating_sub(effective);
            if needed > 0 {
                let _ = self.request_tokens(needed).await;
            }
        }
    }

    async fn request_tokens(&self, n: usize) -> Result<(), JDCError<error::ChannelManager>> {
        if n == 0 {
            return Ok(());
        }

        self.pending_count.fetch_add(n, Ordering::Relaxed);
        debug!("Requesting {} tokens from JDS", n);

        let mut allocate_job = AllocateMiningJobToken {
            user_identifier: self
                .user_identity
                .to_string()
                .try_into()
                .expect("user_identity is a valid SV2 string"),
            request_id: 0,
        };

        let mut sent = 0usize;
        loop {
            let request_id = self.request_id_factory.fetch_add(1, Ordering::Relaxed);
            allocate_job.request_id = request_id;
            let message = JobDeclaration::AllocateMiningJobToken(allocate_job.clone());

            if self.jd_sender.send(message).await.is_err() {
                let not_sent = n - sent;
                self.pending_count.fetch_sub(not_sent, Ordering::Relaxed);
                info!("Failed to send AllocateMiningJobToken - JD channel closed");
                return Err(JDCError::fallback(JDCErrorKind::ChannelErrorSender));
            }
            sent += 1;

            if sent >= n {
                break;
            }
        }

        info!("Requested {} mining job tokens from JDS", n);
        Ok(())
    }
}
