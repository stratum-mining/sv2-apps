//! Monitoring integration for JD Client
//!
//! This module implements the ServerMonitoring and Sv2ClientsMonitoring traits on `ChannelManager`.
//! JDC has:
//! - Server channels (upstream to pool)
//! - Client channels (downstream miners connecting to JDC)

use hex;
use stratum_apps::monitoring::{
    client::{ExtendedChannelInfo, StandardChannelInfo, Sv2ClientInfo, Sv2ClientsMonitoring},
    server::{ServerExtendedChannelInfo, ServerInfo, ServerMonitoring},
};

use crate::{channel_manager::ChannelManager, downstream::Downstream};

impl ServerMonitoring for ChannelManager {
    fn get_server(&self) -> ServerInfo {
        self.channel_manager_data
            .safe_lock(|d| {
                let mut extended_channels = Vec::new();
                let standard_channels = Vec::new(); // JDC only uses extended channels

                if let Some(upstream_channel) = &d.upstream_channel {
                    let channel_id = upstream_channel.get_channel_id();
                    let target = upstream_channel.get_target();
                    let extranonce_prefix = upstream_channel.get_extranonce_prefix();
                    let user_identity = upstream_channel.get_user_identity();
                    let share_accounting = upstream_channel.get_share_accounting();

                    extended_channels.push(ServerExtendedChannelInfo {
                        channel_id,
                        user_identity: user_identity.clone(),
                        nominal_hashrate: Some(upstream_channel.get_nominal_hashrate()),
                        target_hex: hex::encode(target.to_be_bytes()),
                        extranonce_prefix_hex: hex::encode(extranonce_prefix),
                        full_extranonce_size: upstream_channel.get_full_extranonce_size(),
                        rollable_extranonce_size: upstream_channel.get_rollable_extranonce_size(),
                        version_rolling: upstream_channel.is_version_rolling(),
                        shares_acknowledged: share_accounting.get_acknowledged_shares(),
                        shares_submitted: share_accounting.get_validated_shares(),
                        shares_rejected: share_accounting.get_rejected_shares(),
                        share_work_sum: share_accounting.get_share_work_sum(),
                        best_diff: share_accounting.get_best_diff(),
                        blocks_found: share_accounting.get_blocks_found(),
                    });
                }

                ServerInfo {
                    extended_channels,
                    standard_channels,
                }
            })
            .unwrap_or_else(|_| ServerInfo {
                extended_channels: Vec::new(),
                standard_channels: Vec::new(),
            })
    }
}

/// Helper to convert a Downstream to Sv2ClientInfo.
/// Returns None if the lock cannot be acquired (graceful degradation for monitoring).
fn downstream_to_sv2_client_info(client: &Downstream) -> Option<Sv2ClientInfo> {
    client
        .downstream_data
        .safe_lock(|dd| {
            let mut extended_channels = Vec::new();
            let mut standard_channels = Vec::new();

            for (_channel_id, extended_channel) in dd.extended_channels.iter() {
                let channel_id = extended_channel.get_channel_id();
                let target = extended_channel.get_target();
                let requested_max_target = extended_channel.get_requested_max_target();
                let user_identity = extended_channel.get_user_identity();
                let share_accounting = extended_channel.get_share_accounting();

                extended_channels.push(ExtendedChannelInfo {
                    channel_id,
                    user_identity: user_identity.clone(),
                    nominal_hashrate: extended_channel.get_nominal_hashrate(),
                    target_hex: hex::encode(target.to_be_bytes()),
                    requested_max_target_hex: hex::encode(requested_max_target.to_be_bytes()),
                    extranonce_prefix_hex: hex::encode(extended_channel.get_extranonce_prefix()),
                    full_extranonce_size: extended_channel.get_full_extranonce_size(),
                    rollable_extranonce_size: extended_channel.get_rollable_extranonce_size(),
                    expected_shares_per_minute: extended_channel.get_shares_per_minute(),
                    shares_accepted: share_accounting.get_shares_accepted(),
                    share_work_sum: share_accounting.get_share_work_sum(),
                    last_share_sequence_number: share_accounting.get_last_share_sequence_number(),
                    best_diff: share_accounting.get_best_diff(),
                    last_batch_accepted: share_accounting.get_last_batch_accepted(),
                    last_batch_work_sum: share_accounting.get_last_batch_work_sum(),
                    share_batch_size: share_accounting.get_share_batch_size(),
                    blocks_found: share_accounting.get_blocks_found(),
                });
            }

            for (_channel_id, standard_channel) in dd.standard_channels.iter() {
                let channel_id = standard_channel.get_channel_id();
                let target = standard_channel.get_target();
                let requested_max_target = standard_channel.get_requested_max_target();
                let user_identity = standard_channel.get_user_identity();
                let share_accounting = standard_channel.get_share_accounting();

                standard_channels.push(StandardChannelInfo {
                    channel_id,
                    user_identity: user_identity.clone(),
                    nominal_hashrate: standard_channel.get_nominal_hashrate(),
                    target_hex: hex::encode(target.to_be_bytes()),
                    requested_max_target_hex: hex::encode(requested_max_target.to_be_bytes()),
                    extranonce_prefix_hex: hex::encode(standard_channel.get_extranonce_prefix()),
                    expected_shares_per_minute: standard_channel.get_shares_per_minute(),
                    shares_accepted: share_accounting.get_shares_accepted(),
                    share_work_sum: share_accounting.get_share_work_sum(),
                    last_share_sequence_number: share_accounting.get_last_share_sequence_number(),
                    best_diff: share_accounting.get_best_diff(),
                    last_batch_accepted: share_accounting.get_last_batch_accepted(),
                    last_batch_work_sum: share_accounting.get_last_batch_work_sum(),
                    share_batch_size: share_accounting.get_share_batch_size(),
                    blocks_found: share_accounting.get_blocks_found(),
                });
            }

            Sv2ClientInfo {
                client_id: client.downstream_id,
                extended_channels,
                standard_channels,
            }
        })
        .ok()
}

impl Sv2ClientsMonitoring for ChannelManager {
    fn get_sv2_clients(&self) -> Vec<Sv2ClientInfo> {
        // Clone Downstream references and release lock immediately to avoid contention
        // with template distribution and message handling
        let downstream_refs: Vec<Downstream> = self
            .channel_manager_data
            .safe_lock(|data| data.downstream.values().cloned().collect())
            .unwrap_or_default();

        downstream_refs
            .iter()
            .filter_map(downstream_to_sv2_client_info)
            .collect()
    }

    fn get_sv2_client_by_id(&self, client_id: usize) -> Option<Sv2ClientInfo> {
        self.channel_manager_data
            .safe_lock(|d| {
                d.downstream
                    .get(&client_id)
                    .and_then(downstream_to_sv2_client_info)
            })
            .unwrap_or(None)
    }
}
