use std::{
    convert::From,
    fmt::{self, Debug, Formatter},
    marker::PhantomData,
    sync::{MutexGuard, PoisonError},
};

use stratum_apps::{
    stratum_core::{
        binary_sv2, bitcoin,
        channels_sv2::{
            server::{
                error::{ExtendedChannelError, GroupChannelError, StandardChannelError},
                share_accounting::ShareValidationError,
            },
            vardiff::error::VardiffError,
        },
        codec_sv2, framing_sv2,
        handlers_sv2::HandlerErrorType,
        mining_sv2::ExtendedExtranonceError,
        noise_sv2,
        parsers_sv2::{Mining, ParserError},
    },
    utils::types::{
        CanDisconnect, CanShutdown, ChannelId, DownstreamId, ExtensionType, MessageType,
    },
};

pub type PoolResult<T, Owner> = Result<T, PoolError<Owner>>;

#[derive(Debug)]
pub struct ChannelManager;

#[derive(Debug)]
pub struct TemplateProvider;

#[derive(Debug)]
pub struct Downstream;

#[derive(Debug)]
pub struct PoolError<Owner> {
    pub kind: PoolErrorKind,
    pub action: Action,
    _owner: PhantomData<Owner>,
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Log,
    Disconnect(DownstreamId),
    Shutdown,
}

impl CanDisconnect for Downstream {}
impl CanDisconnect for ChannelManager {}

impl CanShutdown for ChannelManager {}
impl CanShutdown for TemplateProvider {}
impl CanShutdown for Downstream {}

impl<O> PoolError<O> {
    pub fn log<E: Into<PoolErrorKind>>(kind: E) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Log,
            _owner: PhantomData,
        }
    }
}

impl<O> PoolError<O>
where
    O: CanDisconnect,
{
    pub fn disconnect<E: Into<PoolErrorKind>>(kind: E, downstream_id: DownstreamId) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Disconnect(downstream_id),
            _owner: PhantomData,
        }
    }
}

impl<O> PoolError<O>
where
    O: CanShutdown,
{
    pub fn shutdown<E: Into<PoolErrorKind>>(kind: E) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Shutdown,
            _owner: PhantomData,
        }
    }
}

impl<Owner> From<PoolError<Owner>> for PoolErrorKind {
    fn from(value: PoolError<Owner>) -> Self {
        value.kind
    }
}

#[derive(Debug)]
pub enum ChannelSv2Error {
    ExtendedChannelServerSide(ExtendedChannelError),
    StandardChannelServerSide(StandardChannelError),
    GroupChannelServerSide(GroupChannelError),
    ExtranonceError(ExtendedExtranonceError),
    ShareValidationError(ShareValidationError),
}

/// Represents various errors that can occur in the pool implementation.
#[derive(std::fmt::Debug)]
pub enum PoolErrorKind {
    /// I/O-related error.
    Io(std::io::Error),
    ChannelSv2(ChannelSv2Error),
    /// Error when sending a message through a channel.
    ChannelSend(Box<dyn std::marker::Send + Debug>),
    /// Error when receiving a message from an asynchronous channel.
    ChannelRecv(async_channel::RecvError),
    /// Error from the `binary_sv2` crate.
    BinarySv2(binary_sv2::Error),
    /// Error from the `codec_sv2` crate.
    Codec(codec_sv2::Error),
    /// Error related to parsing a coinbase output specification.
    CoinbaseOutput(stratum_apps::config_helpers::CoinbaseOutputError),
    /// Error from the `noise_sv2` crate.
    Noise(noise_sv2::Error),
    /// Error related to SV2 message framing.
    Framing(framing_sv2::Error),
    /// Error due to a poisoned lock, typically from a failed mutex operation.
    PoisonLock(String),
    /// Error indicating that a component has shut down unexpectedly.
    ComponentShutdown(String),
    /// Custom error message.
    Custom(String),
    /// Error related to the SV2 protocol, including an error code and a `Mining` message.
    Sv2ProtocolError((u32, Mining<'static>)),
    /// Vardiff Error
    Vardiff(VardiffError),
    /// Parser Error
    Parser(ParserError),
    /// Unexpected message
    UnexpectedMessage(ExtensionType, MessageType),
    /// Channel error sender
    ChannelErrorSender,
    /// Invalid socket address
    InvalidSocketAddress(String),
    /// Bitcoin Encode Error
    BitcoinEncodeError(bitcoin::consensus::encode::Error),
    /// Downstream not found for the channel
    DownstreamNotFoundWithChannelId(ChannelId),
    /// Downstream not found
    DownstreamNotFound(usize),
    /// Downstream Id not found
    DownstreamIdNotFound,
    /// Future template not present
    FutureTemplateNotPresent,
    /// Last new prevhash not found
    LastNewPrevhashNotFound,
    /// Vardiff associated to channel not found
    VardiffNotFound(ChannelId),
    /// Errors on bad `String` to `int` conversion.
    ParseInt(std::num::ParseIntError),
    /// Invalid unsupported extensions sequence
    InvalidUnsupportedExtensionsSequence(binary_sv2::Error),
    /// Invalid required extensions sequence
    InvalidRequiredExtensionsSequence(binary_sv2::Error),
    /// Invalid supported extensions sequence
    InvalidSupportedExtensionsSequence(binary_sv2::Error),
    /// Client does not support required extensions
    ClientDoesNotSupportRequiredExtensions(Vec<u16>),
    /// Failed to create BitcoinCore tokio runtime
    FailedToCreateBitcoinCoreTokioRuntime,
    /// Failed to send CoinbaseOutputConstraints message
    FailedToSendCoinbaseOutputConstraints,
    /// BitcoinCoreSv2TDP cancellation token activated
    BitcoinCoreSv2TDPCancellationTokenActivated,
    /// Unsupported Protocol
    UnsupportedProtocol,
    /// Setup connection error
    SetupConnectionError,
    /// endpoint change error
    ChangeEndpoint,
    /// Could not initiate subsystem
    CouldNotInitiateSystem,
    /// Configuration error
    Configuration(String),
    /// Job not found
    JobNotFound,
    /// Invalid Key
    InvalidKey,
    ///Error when parsing the PayoutMode for Solo Mining Mode
    PayoutModeError(String),
    /// JDS error (from embedded Job Declaration Server)
    Jds(jd_server_sv2::error::JDSErrorKind),
}

impl std::fmt::Display for PoolErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use PoolErrorKind::*;
        match self {
            Io(e) => write!(f, "I/O error: `{e:?}"),
            ChannelSend(e) => write!(f, "Channel send failed: `{e:?}`"),
            ChannelRecv(e) => write!(f, "Channel recv failed: `{e:?}`"),
            BinarySv2(e) => write!(f, "Binary SV2 error: `{e:?}`"),
            Codec(e) => write!(f, "Codec SV2 error: `{e:?}"),
            CoinbaseOutput(e) => write!(f, "Coinbase output error: `{e:?}"),
            Framing(e) => write!(f, "Framing SV2 error: `{e:?}`"),
            Noise(e) => write!(f, "Noise SV2 error: `{e:?}"),
            PoisonLock(e) => write!(f, "Poison lock: {e:?}"),
            ComponentShutdown(e) => write!(f, "Component shutdown: {e:?}"),
            Custom(e) => write!(f, "Custom SV2 error: `{e:?}`"),
            Sv2ProtocolError(e) => {
                write!(f, "Received Sv2 Protocol Error from upstream: `{e:?}`")
            }
            PoolErrorKind::Vardiff(e) => {
                write!(f, "Received Vardiff Error : {e:?}")
            }
            Parser(e) => write!(f, "Parser error: `{e:?}`"),
            UnexpectedMessage(extension_type, message_type) => write!(f, "Unexpected message: extension type: {extension_type:?}, message type: {message_type:?}"),
            ChannelErrorSender => write!(f, "Channel sender error"),
            InvalidSocketAddress(address) => write!(f, "Invalid socket address: {address:?}"),
            BitcoinEncodeError(_) => write!(f, "Error generated during encoding"),
            DownstreamNotFoundWithChannelId(channel_id) => {
                write!(f, "Downstream not found for channel id: {channel_id}")
            }
            DownstreamNotFound(downstream_id) => write!(
                f,
                "Downstream not found with downstream id: {downstream_id}"
            ),
            DownstreamIdNotFound => write!(f, "Downstream id not found"),
            FutureTemplateNotPresent => write!(f, "future template not present"),
            LastNewPrevhashNotFound => write!(f, "last prev hash not present"),
            VardiffNotFound(downstream_id) => write!(
                f,
                "Vardiff not found available for downstream id: {downstream_id}"
            ),
            ParseInt(e) => write!(f, "Conversion error: {e:?}"),
            ChannelSv2(channel_error) => {
                write!(f, "Channel error: {channel_error:?}")
            }
            InvalidUnsupportedExtensionsSequence(e) => {
                write!(
                    f,
                    "Invalid unsupported extensions sequence: {e:?}"
                )
            }
            InvalidRequiredExtensionsSequence(e) => {
                write!(
                    f,
                    "Invalid required extensions sequence: {e:?}"
                )
            }
            InvalidSupportedExtensionsSequence(e) => {
                write!(
                    f,
                    "Invalid supported extensions sequence: {e:?}"
                )
            }
            ClientDoesNotSupportRequiredExtensions(extensions) => {
                write!(
                    f,
                    "Client does not support required extensions: {extensions:?}"
                )
            }
            FailedToCreateBitcoinCoreTokioRuntime => {
                write!(f, "Failed to create BitcoinCore tokio runtime")
            }
            FailedToSendCoinbaseOutputConstraints => {
                write!(f, "Failed to send CoinbaseOutputConstraints message")
            }
            BitcoinCoreSv2TDPCancellationTokenActivated => {
                write!(f, "BitcoinCoreSv2TDP cancellation token activated")
            },
            UnsupportedProtocol => write!(f, "Protocol not supported"),
            SetupConnectionError => {
                write!(f, "Failed to Setup connection")
            }
            ChangeEndpoint => {
                write!(f, "Change endpoint")
            }
            CouldNotInitiateSystem => write!(f, "Could not initiate subsystem"),
            Configuration(e) => write!(f, "Configuration error: {e}"),
            JobNotFound => write!(f, "Job not found"),
            InvalidKey => write!(f, "Invalid key used during noise handshake"),
            PayoutModeError(e) => write!(f, "Unable to parse the PayoutMode: {e}"),
            Jds(e) => write!(f, "JDS error: {e:?}"),
        }
    }
}

impl From<std::io::Error> for PoolErrorKind {
    fn from(e: std::io::Error) -> PoolErrorKind {
        PoolErrorKind::Io(e)
    }
}

impl From<async_channel::RecvError> for PoolErrorKind {
    fn from(e: async_channel::RecvError) -> PoolErrorKind {
        PoolErrorKind::ChannelRecv(e)
    }
}

impl From<binary_sv2::Error> for PoolErrorKind {
    fn from(e: binary_sv2::Error) -> PoolErrorKind {
        PoolErrorKind::BinarySv2(e)
    }
}

impl From<codec_sv2::Error> for PoolErrorKind {
    fn from(e: codec_sv2::Error) -> PoolErrorKind {
        PoolErrorKind::Codec(e)
    }
}

impl From<stratum_apps::config_helpers::CoinbaseOutputError> for PoolErrorKind {
    fn from(e: stratum_apps::config_helpers::CoinbaseOutputError) -> PoolErrorKind {
        PoolErrorKind::CoinbaseOutput(e)
    }
}

impl From<noise_sv2::Error> for PoolErrorKind {
    fn from(e: noise_sv2::Error) -> PoolErrorKind {
        PoolErrorKind::Noise(e)
    }
}

impl<T: 'static + std::marker::Send + Debug> From<async_channel::SendError<T>> for PoolErrorKind {
    fn from(e: async_channel::SendError<T>) -> PoolErrorKind {
        PoolErrorKind::ChannelSend(Box::new(e))
    }
}

impl From<String> for PoolErrorKind {
    fn from(e: String) -> PoolErrorKind {
        PoolErrorKind::Custom(e)
    }
}
impl From<framing_sv2::Error> for PoolErrorKind {
    fn from(e: framing_sv2::Error) -> PoolErrorKind {
        PoolErrorKind::Framing(e)
    }
}

impl<T> From<PoisonError<MutexGuard<'_, T>>> for PoolErrorKind {
    fn from(e: PoisonError<MutexGuard<T>>) -> PoolErrorKind {
        PoolErrorKind::PoisonLock(e.to_string())
    }
}

impl From<(u32, Mining<'static>)> for PoolErrorKind {
    fn from(e: (u32, Mining<'static>)) -> Self {
        PoolErrorKind::Sv2ProtocolError(e)
    }
}

impl From<stratum_apps::stratum_core::bitcoin::consensus::encode::Error> for PoolErrorKind {
    fn from(value: stratum_apps::stratum_core::bitcoin::consensus::encode::Error) -> Self {
        PoolErrorKind::BitcoinEncodeError(value)
    }
}

impl From<ExtendedChannelError> for PoolErrorKind {
    fn from(value: ExtendedChannelError) -> Self {
        PoolErrorKind::ChannelSv2(ChannelSv2Error::ExtendedChannelServerSide(value))
    }
}

impl From<StandardChannelError> for PoolErrorKind {
    fn from(value: StandardChannelError) -> Self {
        PoolErrorKind::ChannelSv2(ChannelSv2Error::StandardChannelServerSide(value))
    }
}

impl From<GroupChannelError> for PoolErrorKind {
    fn from(value: GroupChannelError) -> Self {
        PoolErrorKind::ChannelSv2(ChannelSv2Error::GroupChannelServerSide(value))
    }
}

impl From<ExtendedExtranonceError> for PoolErrorKind {
    fn from(value: ExtendedExtranonceError) -> Self {
        PoolErrorKind::ChannelSv2(ChannelSv2Error::ExtranonceError(value))
    }
}

impl From<VardiffError> for PoolErrorKind {
    fn from(value: VardiffError) -> Self {
        PoolErrorKind::Vardiff(value)
    }
}

impl From<ParserError> for PoolErrorKind {
    fn from(value: ParserError) -> Self {
        PoolErrorKind::Parser(value)
    }
}

impl From<ShareValidationError> for PoolErrorKind {
    fn from(value: ShareValidationError) -> Self {
        PoolErrorKind::ChannelSv2(ChannelSv2Error::ShareValidationError(value))
    }
}

impl From<jd_server_sv2::error::JDSErrorKind> for PoolErrorKind {
    fn from(value: jd_server_sv2::error::JDSErrorKind) -> Self {
        PoolErrorKind::Jds(value)
    }
}

impl<Owner> HandlerErrorType for PoolError<Owner> {
    fn parse_error(error: ParserError) -> Self {
        Self {
            kind: PoolErrorKind::Parser(error),
            action: Action::Log,
            _owner: PhantomData,
        }
    }

    fn unexpected_message(extension_type: ExtensionType, message_type: MessageType) -> Self {
        Self {
            kind: PoolErrorKind::UnexpectedMessage(extension_type, message_type),
            action: Action::Log,
            _owner: PhantomData,
        }
    }
}

impl<Owner> std::fmt::Display for PoolError<Owner> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}/{:?}]", self.kind, self.action)
    }
}
