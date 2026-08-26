use stratum_core::{
    codec_sv2::{EncodableFrame, StandardSerializedFrame},
    parsers_sv2::AnyMessageOwned,
};

pub const GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS: u64 = 5;

/// The SV2 protocol version supported by the apps in this workspace.
pub const SUPPORTED_PROTOCOL_VERSION: u16 = 2;

pub type TemplateId = u64;
pub type UpstreamJobId = u32;
pub type JobId = u32;
pub type DownstreamId = usize;
pub type RequestId = u32;
pub type ChannelId = u32;
pub type Hashrate = f32;
pub type SharesPerMinute = f32;
pub type SharesBatchSize = usize;
pub type ExtensionType = u16;
pub type MessageType = u8;
pub type JdToken = u64;

pub type Message = AnyMessageOwned;

/// A frame on its way out, carrying a message still to be serialized.
pub type Sv2Frame = stratum_core::codec_sv2::Sv2Frame<Message>;

/// A frame on its way in, carrying the bytes read off the wire.
pub type SerializedFrame = StandardSerializedFrame;

/// A frame on its way out, for a sender that has either kind to send: a message the encoder will
/// serialize, or bytes that were framed by hand, as the TLV path does.
#[derive(Debug)]
pub enum OutboundFrame {
    /// A message the encoder serializes on the way out.
    Message(Sv2Frame),

    /// A frame that is already serialized, written out as it is.
    Serialized(SerializedFrame),
}

impl From<Sv2Frame> for OutboundFrame {
    fn from(frame: Sv2Frame) -> Self {
        Self::Message(frame)
    }
}

impl From<SerializedFrame> for OutboundFrame {
    fn from(frame: SerializedFrame) -> Self {
        Self::Serialized(frame)
    }
}

impl EncodableFrame for OutboundFrame {
    fn encoded_length(&self) -> usize {
        match self {
            Self::Message(frame) => frame.encoded_length(),
            Self::Serialized(frame) => frame.encoded_length(),
        }
    }

    fn encode_into(self, dst: &mut [u8]) -> Result<(), stratum_core::framing_sv2::Error> {
        match self {
            Self::Message(frame) => frame.encode_into(dst),
            Self::Serialized(frame) => frame.encode_into(dst),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VardiffKey {
    pub downstream_id: DownstreamId,
    pub channel_id: ChannelId,
}

impl From<(DownstreamId, ChannelId)> for VardiffKey {
    fn from(value: (DownstreamId, ChannelId)) -> Self {
        VardiffKey {
            downstream_id: value.0,
            channel_id: value.1,
        }
    }
}

/// Marker traits used to define set of action
/// the implementor can take
pub trait CanDisconnect {}
pub trait CanFallback {}
pub trait CanShutdown {}
