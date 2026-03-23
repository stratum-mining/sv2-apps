//! Monitoring integration for Pool
//!
//! This module implements the Sv2ClientsMonitoring trait on `ChannelManager`.
//! Pool only has clients (miners connecting to it), no upstream server.

use stratum_apps::monitoring::client::{
    ExtendedChannelInfo, StandardChannelInfo, Sv2ClientInfo, Sv2ClientsMonitoring,
};

use crate::{channel_manager::ChannelManager, downstream::Downstream};

/// Helper to convert a Downstream to Sv2ClientInfo.
/// Returns None if the lock cannot be acquired (graceful degradation for monitoring).
fn downstream_to_sv2_client_info(client: &Downstream) -> Sv2ClientInfo {
    let mut extended_channels = Vec::new();
    let mut standard_channels = Vec::new();

    client
        .extended_channels
        .for_each(|channel_id, extended_channel| {
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
        });

    client
        .standard_channels
        .for_each(|channel_id, standard_channel| {
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
        });

    Sv2ClientInfo {
        client_id: client.downstream_id,
        extended_channels,
        standard_channels,
    }
}

impl Sv2ClientsMonitoring for ChannelManager {
    fn get_sv2_clients(&self) -> Vec<Sv2ClientInfo> {
        // Clone Downstream references and release lock immediately to avoid contention
        // with template distribution and message handling
        let mut downstream_refs: Vec<Downstream> = Vec::new();
        self.downstream
            .for_each(|_id, downstream| downstream_refs.push(downstream.clone()));

        downstream_refs
            .iter()
            .map(downstream_to_sv2_client_info)
            .collect()
    }

    fn get_sv2_client_by_id(&self, client_id: usize) -> Option<Sv2ClientInfo> {
        self.downstream
            .with(&client_id, downstream_to_sv2_client_info)
    }
}
