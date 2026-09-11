use stratum_apps::stratum_core::codec_sv2::SerializedFrame;

/// A frame received from a role under test: a header plus the raw payload bytes behind it.
pub type InboundFrame = SerializedFrame;
pub type MsgType = u8;
