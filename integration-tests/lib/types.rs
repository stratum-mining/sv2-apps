use stratum_apps::stratum_core::codec_sv2::StandardSerializedFrame;

/// A frame received from a role under test.
pub type MessageFrame = StandardSerializedFrame;
pub type MsgType = u8;
