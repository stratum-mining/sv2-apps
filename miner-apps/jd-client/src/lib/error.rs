//! ## Error Module
//!
//! Defines [`Error`], the central error enum used throughout the Job Declarator Client (JDC).
//!
//! It unifies errors from:
//! - I/O operations
//! - Channels (send/recv)
//! - SV2 stack: Binary, Codec, Noise, Framing, RolesLogic
//! - Locking logic (PoisonError)
//! - Domain-specific issues
//!
//! This module ensures that all errors can be passed around consistently, including across async
//! boundaries.
use ext_config::ConfigError;
use std::{
    fmt::{self, Formatter},
    marker::PhantomData,
};
use stratum_apps::{
    network_helpers,
    stratum_core::{
        binary_sv2, bitcoin,
        channels_sv2::{
            client::error::ExtendedChannelError as ExtendedChannelClientError,
            server::error::{
                ExtendedChannelError as ExtendedChannelServerError, GroupChannelError,
                StandardChannelError,
            },
        },
        framing_sv2,
        handlers_sv2::HandlerErrorType,
        mining_sv2::ExtendedExtranonceError,
        noise_sv2,
        parsers_sv2::ParserError,
    },
    utils::types::{
        CanDisconnect, CanFallback, CanShutdown, DownstreamId, ExtensionType, JobId, MessageType,
        RequestId, TemplateId, VardiffKey,
    },
};
use tokio::{sync::broadcast, time::error::Elapsed};

pub type JDCResult<T, Owner> = Result<T, JDCError<Owner>>;

#[derive(Debug)]
pub struct ChannelManager;

#[derive(Debug)]
pub struct TemplateProvider;

#[derive(Debug)]
pub struct JobDeclarator;

#[derive(Debug)]
pub struct Upstream;

#[derive(Debug)]
pub struct Downstream;

#[derive(Debug)]
pub struct JDCError<Owner> {
    pub kind: JDCErrorKind,
    pub action: Action,
    _owner: PhantomData<Owner>,
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Log,
    Disconnect(DownstreamId),
    Fallback,
    Shutdown,
}

impl CanDisconnect for Downstream {}
impl CanDisconnect for ChannelManager {}

impl CanFallback for Upstream {}
impl CanFallback for JobDeclarator {}
impl CanFallback for ChannelManager {}

impl CanShutdown for ChannelManager {}
impl CanShutdown for TemplateProvider {}
impl CanShutdown for Downstream {}
impl CanShutdown for Upstream {}
impl CanShutdown for JobDeclarator {}

impl<O> JDCError<O> {
    pub fn log<E: Into<JDCErrorKind>>(kind: E) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Log,
            _owner: PhantomData,
        }
    }
}

impl<O> JDCError<O>
where
    O: CanDisconnect,
{
    pub fn disconnect<E: Into<JDCErrorKind>>(kind: E, downstream_id: DownstreamId) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Disconnect(downstream_id),
            _owner: PhantomData,
        }
    }
}

impl<O> JDCError<O>
where
    O: CanFallback,
{
    pub fn fallback<E: Into<JDCErrorKind>>(kind: E) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Fallback,
            _owner: PhantomData,
        }
    }
}

impl<O> JDCError<O>
where
    O: CanShutdown,
{
    pub fn shutdown<E: Into<JDCErrorKind>>(kind: E) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Shutdown,
            _owner: PhantomData,
        }
    }
}

#[derive(Debug)]
pub enum ChannelSv2Error {
    ExtendedChannelClientSide(ExtendedChannelClientError),
    ExtendedChannelServerSide(ExtendedChannelServerError),
    ExtranonceError(ExtendedExtranonceError),
    StandardChannelServerSide(StandardChannelError),
    GroupChannelServerSide(GroupChannelError),
}

#[derive(Debug)]
pub enum JDCErrorKind {
    /// Errors on bad CLI argument input.
    BadCliArgs,
    /// Errors on bad `config` TOML deserialize.
    BadConfigDeserialize(ConfigError),
    /// Errors from `binary_sv2` crate.
    BinarySv2(binary_sv2::Error),
    /// Errors on bad noise handshake.
    CodecNoise(noise_sv2::Error),
    /// Errors from `framing_sv2` crate.
    FramingSv2(framing_sv2::Error),
    /// Errors on bad `TcpStream` connection.
    Io(std::io::Error),
    /// Errors on bad `String` to `int` conversion.
    ParseInt(std::num::ParseIntError),
    Parser(ParserError),
    /// Channel receiver error
    ChannelErrorReceiver(async_channel::RecvError),
    /// Channel sender error
    ChannelErrorSender,
    /// Broadcast channel receiver error
    BroadcastChannelErrorReceiver(broadcast::error::RecvError),
    /// Network helpers error
    NetworkHelpersError(network_helpers::Error),
    /// Unexpected message
    UnexpectedMessage(ExtensionType, MessageType),
    /// Invalid user identity
    InvalidUserIdentity(String),
    /// Bitcoin encode error
    BitcoinEncodeError(bitcoin::consensus::encode::Error),
    /// Invalid socket address
    InvalidSocketAddress(String),
    /// Timeout error
    Timeout,
    /// Declared job corresponding to request Id not found.
    LastDeclareJobNotFound(RequestId),
    /// No active job with job id
    ActiveJobNotFound(JobId),
    /// Template not found with template ID
    TemplateNotFound(TemplateId),
    /// Downstream not found with downstream ID
    DownstreamNotFound(DownstreamId),
    /// Future template not present
    FutureTemplateNotPresent,
    /// Last new prevhash not found
    LastNewPrevhashNotFound,
    /// Vardiff not found
    VardiffNotFound(VardiffKey),
    /// Tx data error
    TxDataError,
    /// Frame conversion error
    FrameConversionError,
    /// Failed to create custom Job
    FailedToCreateCustomJob,
    /// Allocate Mining job token coinbase output error
    AllocateMiningJobTokenSuccessCoinbaseOutputsError,
    /// Channel manager has bad coinbase outputs.
    ChannelManagerHasBadCoinbaseOutputs,
    /// Declared job has bad coinbase outputs.
    DeclaredJobHasBadCoinbaseOutputs,
    /// Extranonce size is too large
    ExtranonceSizeTooLarge,
    /// Extranonce size is too small
    ExtranonceSizeTooSmall,
    /// Could not create group channel
    FailedToCreateGroupChannel(GroupChannelError),
    ///Channel Errors
    ChannelSv2(ChannelSv2Error),
    /// Extranonce prefix error
    ExtranoncePrefixFactoryError(ExtendedExtranonceError),
    /// Invalid unsupported extensions sequence (exceeds maximum length)
    InvalidUnsupportedExtensionsSequence,
    /// Invalid required extensions sequence (exceeds maximum length)
    InvalidRequiredExtensionsSequence,
    /// Invalid supported extensions sequence (exceeds maximum length)
    InvalidSupportedExtensionsSequence,
    /// Server does not support required extensions
    RequiredExtensionsNotSupported(Vec<u16>),
    /// Server requires extensions that the translator doesn't support
    ServerRequiresUnsupportedExtensions(Vec<u16>),
    /// BitcoinCoreSv2TDP cancellation token activated
    BitcoinCoreSv2TDPCancellationTokenActivated,
    /// Failed to create BitcoinCoreSv2TDP tokio runtime
    FailedToCreateBitcoinCoreTokioRuntime,
    /// Failed to send CoinbaseOutputConstraints message
    FailedToSendCoinbaseOutputConstraints,
    /// Setup Connection Error
    SetupConnectionError,
    /// Endpoint changed
    ChangeEndpoint,
    /// Received upstream message during solo mining
    UpstreamMessageDuringSoloMining,
    /// Declare mining job error
    DeclareMiningJobError,
    /// Channel opening error
    OpenMiningChannelError,
    /// Standard channel opening error
    OpenStandardMiningChannelError,
    /// Close channel
    CloseChannel,
    /// Custom job error
    CustomJobError,
    /// Could not initiate subsystem
    CouldNotInitiateSystem,
    /// Invalid key
    InvalidKey,
    /// Upstream not found
    UpstreamNotFound,
}

impl std::error::Error for JDCErrorKind {}

impl fmt::Display for JDCErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use JDCErrorKind::*;
        match self {
            BadCliArgs => write!(f, "Bad CLI arg input"),
            BadConfigDeserialize(ref e) => write!(f, "Bad `config` TOML deserialize: `{e:?}`"),
            BinarySv2(ref e) => write!(f, "Binary SV2 error: `{e:?}`"),
            CodecNoise(ref e) => write!(f, "Noise error: `{e:?}"),
            FramingSv2(ref e) => write!(f, "Framing SV2 error: `{e:?}`"),
            Io(ref e) => write!(f, "I/O error: `{e:?}"),
            ParseInt(ref e) => write!(f, "Bad convert from `String` to `int`: `{e:?}`"),
            ChannelErrorReceiver(ref e) => write!(f, "Channel receive error: `{e:?}`"),
            Parser(ref e) => write!(f, "Parser error: `{e:?}`"),
            BroadcastChannelErrorReceiver(ref e) => {
                write!(f, "Broadcast channel receive error: {e:?}")
            }
            ChannelErrorSender => write!(f, "Sender error"),
            NetworkHelpersError(ref e) => write!(f, "Network error: {e:?}"),
            UnexpectedMessage(extension_type, message_type) => {
                write!(f, "Unexpected Message: {extension_type} {message_type}")
            }
            InvalidUserIdentity(_) => write!(f, "User ID is invalid"),
            BitcoinEncodeError(_) => write!(f, "Error generated during encoding"),
            InvalidSocketAddress(ref s) => write!(f, "Invalid socket address: {s}"),
            Timeout => write!(f, "Time out error"),
            LastDeclareJobNotFound(request_id) => {
                write!(f, "last declare job not found for request id: {request_id}")
            }
            ActiveJobNotFound(request_id) => {
                write!(f, "Active Job not found for request_id: {request_id}")
            }
            TemplateNotFound(template_id) => {
                write!(f, "Template not found, template_id: {template_id}")
            }
            DownstreamNotFound(downstream_id) => {
                write!(
                    f,
                    "Downstream not found with downstream_id: {downstream_id}"
                )
            }
            FutureTemplateNotPresent => {
                write!(f, "Future template not present")
            }
            LastNewPrevhashNotFound => {
                write!(f, "Last new prevhash not found")
            }
            VardiffNotFound(vardiff_key) => {
                write!(f, "Vardiff not found for vardiff key: {vardiff_key:?}")
            }
            TxDataError => {
                write!(f, "Transaction data error")
            }
            FrameConversionError => {
                write!(f, "Could not convert message to frame")
            }
            FailedToCreateCustomJob => {
                write!(f, "failed to create custom job")
            }
            AllocateMiningJobTokenSuccessCoinbaseOutputsError => {
                write!(
                    f,
                    "AllocateMiningJobToken.Success coinbase outputs are not deserializable"
                )
            }
            ChannelManagerHasBadCoinbaseOutputs => {
                write!(f, "Channel Manager coinbase outputs are not deserializable")
            }
            DeclaredJobHasBadCoinbaseOutputs => {
                write!(f, "Declared job coinbase outputs are not deserializable")
            }
            ExtranonceSizeTooLarge => {
                write!(f, "Extranonce size too large")
            }
            ExtranonceSizeTooSmall => {
                write!(f, "Extranonce size too small")
            }
            FailedToCreateGroupChannel(ref e) => {
                write!(f, "Failed to create group channel: {e:?}")
            }
            ExtranoncePrefixFactoryError(e) => {
                write!(f, "Failed to create ExtranoncePrefixFactory: {e:?}")
            }
            ChannelSv2(channel_error) => {
                write!(f, "Channel error: {channel_error:?}")
            }
            InvalidUnsupportedExtensionsSequence => {
                write!(
                    f,
                    "Invalid unsupported extensions sequence (exceeds maximum length)"
                )
            }
            InvalidRequiredExtensionsSequence => {
                write!(
                    f,
                    "Invalid required extensions sequence (exceeds maximum length)"
                )
            }
            InvalidSupportedExtensionsSequence => {
                write!(
                    f,
                    "Invalid supported extensions sequence (exceeds maximum length)"
                )
            }
            RequiredExtensionsNotSupported(extensions) => {
                write!(
                    f,
                    "Server does not support required extensions: {extensions:?}"
                )
            }
            ServerRequiresUnsupportedExtensions(extensions) => {
                write!(f, "Server requires extensions that the translator doesn't support: {extensions:?}")
            }
            BitcoinCoreSv2TDPCancellationTokenActivated => {
                write!(f, "BitcoinCoreSv2TDP cancellation token activated")
            }
            FailedToCreateBitcoinCoreTokioRuntime => {
                write!(f, "Failed to create BitcoinCoreSv2TDP tokio runtime")
            }
            FailedToSendCoinbaseOutputConstraints => {
                write!(f, "Failed to send CoinbaseOutputConstraints message")
            }
            SetupConnectionError => {
                write!(f, "Failed to Setup connection")
            }
            ChangeEndpoint => {
                write!(f, "Change endpoint")
            }
            OpenMiningChannelError => write!(f, "failed to open mining channel"),
            OpenStandardMiningChannelError => write!(f, "failed to open standard mining channel"),
            DeclareMiningJobError => write!(f, "job declaration rejected by server"),
            UpstreamMessageDuringSoloMining => {
                write!(f, "received upstream message during solo mining mode")
            }
            CloseChannel => write!(f, "channel closed by upstream"),
            CustomJobError => write!(f, "Custom job not acknowledged"),
            CouldNotInitiateSystem => write!(f, "Could not initiate subsystem"),
            InvalidKey => write!(f, "Invalid key used during noise handshake"),
            UpstreamNotFound => write!(f, "Upstream not found"),
        }
    }
}

impl From<ParserError> for JDCErrorKind {
    fn from(e: ParserError) -> Self {
        JDCErrorKind::Parser(e)
    }
}

impl From<binary_sv2::Error> for JDCErrorKind {
    fn from(e: binary_sv2::Error) -> Self {
        JDCErrorKind::BinarySv2(e)
    }
}

impl From<noise_sv2::Error> for JDCErrorKind {
    fn from(e: noise_sv2::Error) -> Self {
        JDCErrorKind::CodecNoise(e)
    }
}

impl From<framing_sv2::Error> for JDCErrorKind {
    fn from(e: framing_sv2::Error) -> Self {
        JDCErrorKind::FramingSv2(e)
    }
}

impl From<std::io::Error> for JDCErrorKind {
    fn from(e: std::io::Error) -> Self {
        JDCErrorKind::Io(e)
    }
}

impl From<std::num::ParseIntError> for JDCErrorKind {
    fn from(e: std::num::ParseIntError) -> Self {
        JDCErrorKind::ParseInt(e)
    }
}

impl From<ConfigError> for JDCErrorKind {
    fn from(e: ConfigError) -> Self {
        JDCErrorKind::BadConfigDeserialize(e)
    }
}

impl From<async_channel::RecvError> for JDCErrorKind {
    fn from(e: async_channel::RecvError) -> Self {
        JDCErrorKind::ChannelErrorReceiver(e)
    }
}

impl From<network_helpers::Error> for JDCErrorKind {
    fn from(value: network_helpers::Error) -> Self {
        JDCErrorKind::NetworkHelpersError(value)
    }
}

impl From<stratum_apps::stratum_core::bitcoin::consensus::encode::Error> for JDCErrorKind {
    fn from(value: stratum_apps::stratum_core::bitcoin::consensus::encode::Error) -> Self {
        JDCErrorKind::BitcoinEncodeError(value)
    }
}

impl From<Elapsed> for JDCErrorKind {
    fn from(_value: Elapsed) -> Self {
        Self::Timeout
    }
}

impl HandlerErrorType for JDCErrorKind {
    fn parse_error(error: ParserError) -> Self {
        JDCErrorKind::Parser(error)
    }

    fn unexpected_message(extension_type: ExtensionType, message_type: MessageType) -> Self {
        JDCErrorKind::UnexpectedMessage(extension_type, message_type)
    }
}

impl From<ExtendedChannelClientError> for JDCErrorKind {
    fn from(value: ExtendedChannelClientError) -> Self {
        JDCErrorKind::ChannelSv2(ChannelSv2Error::ExtendedChannelClientSide(value))
    }
}

impl From<ExtendedChannelServerError> for JDCErrorKind {
    fn from(value: ExtendedChannelServerError) -> Self {
        JDCErrorKind::ChannelSv2(ChannelSv2Error::ExtendedChannelServerSide(value))
    }
}

impl From<StandardChannelError> for JDCErrorKind {
    fn from(value: StandardChannelError) -> Self {
        JDCErrorKind::ChannelSv2(ChannelSv2Error::StandardChannelServerSide(value))
    }
}

impl From<ExtendedExtranonceError> for JDCErrorKind {
    fn from(value: ExtendedExtranonceError) -> Self {
        JDCErrorKind::ChannelSv2(ChannelSv2Error::ExtranonceError(value))
    }
}

impl From<GroupChannelError> for JDCErrorKind {
    fn from(value: GroupChannelError) -> Self {
        JDCErrorKind::ChannelSv2(ChannelSv2Error::GroupChannelServerSide(value))
    }
}

impl<Owner> HandlerErrorType for JDCError<Owner> {
    fn parse_error(error: ParserError) -> Self {
        Self {
            kind: JDCErrorKind::Parser(error),
            action: Action::Log,
            _owner: PhantomData,
        }
    }

    fn unexpected_message(extension_type: ExtensionType, message_type: MessageType) -> Self {
        Self {
            kind: JDCErrorKind::UnexpectedMessage(extension_type, message_type),
            action: Action::Log,
            _owner: PhantomData,
        }
    }
}

impl<Owner> std::fmt::Display for JDCError<Owner> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}/{:?}]", self.kind, self.action)
    }
}
