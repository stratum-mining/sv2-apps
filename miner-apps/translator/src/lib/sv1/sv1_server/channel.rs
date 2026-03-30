use async_channel::{unbounded, Receiver, Sender};
use std::{collections::HashMap, sync::Arc};
use stratum_apps::{
    custom_mutex::Mutex,
    stratum_core::parsers_sv2::{Mining, Tlv},
};

use stratum_apps::{stratum_core::sv1_api::json_rpc, utils::types::DownstreamId};

#[derive(Clone)]
pub struct Sv1ServerChannelState {
    pub sv1_server_to_downstream_sender:
        Arc<Mutex<HashMap<DownstreamId, Sender<json_rpc::Message>>>>,
    pub downstream_to_sv1_server_sender: Sender<(DownstreamId, json_rpc::Message)>,
    pub downstream_to_sv1_server_receiver: Receiver<(DownstreamId, json_rpc::Message)>,
    pub channel_manager_receiver: Receiver<(Mining<'static>, Option<Vec<Tlv>>)>,
    pub channel_manager_sender: Sender<(Mining<'static>, Option<Vec<Tlv>>)>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Sv1ServerChannelState {
    pub fn new(
        channel_manager_receiver: Receiver<(Mining<'static>, Option<Vec<Tlv>>)>,
        channel_manager_sender: Sender<(Mining<'static>, Option<Vec<Tlv>>)>,
    ) -> Self {
        let (downstream_to_sv1_server_sender, downstream_to_sv1_server_receiver) = unbounded();

        Self {
            sv1_server_to_downstream_sender: Arc::new(Mutex::new(HashMap::new())),
            downstream_to_sv1_server_receiver,
            downstream_to_sv1_server_sender,
            channel_manager_receiver,
            channel_manager_sender,
        }
    }

    pub fn drop(&self) {
        self.channel_manager_receiver.close();
        self.channel_manager_sender.close();
        self.downstream_to_sv1_server_receiver.close();
        self.downstream_to_sv1_server_sender.close();
    }
}
