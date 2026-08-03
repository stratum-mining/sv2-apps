use stratum_apps::stratum_core::{codec_sv2::StandardEitherFrame, parsers_sv2::AnyMessageOwned};

pub type MessageFrame = StandardEitherFrame<AnyMessageOwned>;
pub type MsgType = u8;
