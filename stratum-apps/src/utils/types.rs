use stratum_core::{
    codec_sv2::{EncodableFrame, StandardSerializedFrame, Sv2Frame},
    parsers_sv2::{AnyMessageOwned, ParserError},
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

/// A frame read off the wire: a header plus the raw payload bytes behind it.
pub type InboundFrame = StandardSerializedFrame;

/// A frame on its way out, for a sender that has either kind to send: a message the encoder will
/// serialize, or bytes that were framed by hand, as the TLV path does.
#[derive(Debug)]
pub enum OutboundFrame {
    /// A message the encoder serializes on the way out.
    Message(Sv2Frame<Message>),

    /// A frame that is already serialized, written out as it is.
    Raw(StandardSerializedFrame),
}

impl OutboundFrame {
    /// Frames `message` for the encoder to serialize as it writes it out.
    pub fn from_message(message: Message) -> Result<Self, ParserError> {
        Ok(Self::Message(message.try_into()?))
    }
}

impl From<Sv2Frame<Message>> for OutboundFrame {
    fn from(frame: Sv2Frame<Message>) -> Self {
        Self::Message(frame)
    }
}

impl From<StandardSerializedFrame> for OutboundFrame {
    fn from(frame: StandardSerializedFrame) -> Self {
        Self::Raw(frame)
    }
}

impl EncodableFrame for OutboundFrame {
    fn encoded_length(&self) -> usize {
        match self {
            Self::Message(frame) => frame.encoded_length(),
            Self::Raw(frame) => frame.encoded_length(),
        }
    }

    fn encode_into(self, dst: &mut [u8]) -> Result<(), stratum_core::framing_sv2::Error> {
        match self {
            Self::Message(frame) => frame.encode_into(dst),
            Self::Raw(frame) => frame.encode_into(dst),
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
