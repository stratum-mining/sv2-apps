//! Error types and recovery actions for the Job Declaration Server.
//!
//! Every fallible operation in `jd-server` returns a [`JDSError`] that bundles an
//! [`Action`] describing how the caller should recover:
//!
//! | Action | Meaning |
//! |---|---|
//! | [`Action::Log`] | Non-fatal — log and continue. |
//! | [`Action::Disconnect`] | A specific downstream misbehaved or disconnected — clean it up. |
//! | [`Action::Shutdown`] | Unrecoverable — shut down the whole server. |
//!
//! The `Owner` type parameter is a zero-sized marker (e.g. [`JobDeclarator`], [`Downstream`])
//! that controls which constructors (`shutdown`, `disconnect`) are available at the type level.

use std::{fmt::Debug, marker::PhantomData};

use stratum_apps::{
    stratum_core::{
        binary_sv2, codec_sv2, framing_sv2, handlers_sv2::HandlerErrorType, noise_sv2,
        parsers_sv2::ParserError,
    },
    utils::types::{
        CanDisconnect, CanShutdown, DownstreamId, ExtensionType, MessageType, RequestId,
    },
};

/// Convenience alias for `Result<T, JDSError<Owner>>`.
pub type JDSResult<T, Owner> = Result<T, JDSError<Owner>>;

/// An error paired with an [`Action`] that tells the caller how to recover.
///
/// The `Owner` phantom constrains which constructor methods are available.
#[derive(Debug)]
pub struct JDSError<Owner> {
    pub kind: JDSErrorKind,
    pub action: Action,
    _owner: PhantomData<Owner>,
}

/// Recovery action attached to every [`JDSError`].
#[derive(Debug, Clone, Copy)]
pub enum Action {
    /// Non-fatal — log the error and continue processing.
    Log,
    /// A single downstream client should be cleaned up.
    Disconnect(DownstreamId),
    /// Unrecoverable — the server must shut down.
    Shutdown,
}

/// Marker type for errors originating from the [`crate::job_declarator::JobDeclarator`] layer.
#[derive(Debug)]
pub struct JobDeclarator;

impl CanShutdown for JobDeclarator {}
impl CanDisconnect for JobDeclarator {}

impl<O> JDSError<O>
where
    O: CanShutdown,
{
    /// Constructs a [`JDSError`] with [`Action::Shutdown`].
    pub fn shutdown<E: Into<JDSErrorKind>>(kind: E) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Shutdown,
            _owner: PhantomData,
        }
    }
}

/// Marker type for errors originating from the `Downstream` layer.
#[derive(Debug)]
pub struct Downstream;

impl CanDisconnect for Downstream {}
impl CanShutdown for Downstream {}

impl<O> JDSError<O>
where
    O: CanDisconnect,
{
    /// Constructs a [`JDSError`] with [`Action::Disconnect`] for the given downstream.
    pub fn disconnect<E: Into<JDSErrorKind>>(kind: E, downstream_id: DownstreamId) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Disconnect(downstream_id),
            _owner: PhantomData,
        }
    }
}

/// Underlying error kind without a recovery action.
///
/// Wraps the various protocol, I/O, and domain-specific errors that can occur inside JDS.
#[derive(Debug)]
pub enum JDSErrorKind {
    Io(std::io::Error),
    ChannelSend(Box<dyn std::marker::Send + Debug>),
    ChannelRecv(async_channel::RecvError),
    BinarySv2(binary_sv2::Error),
    Codec(codec_sv2::Error),
    Noise(noise_sv2::Error),
    Parser(ParserError),
    BitcoinCoreIPC(String),
    Framing(framing_sv2::Error),
    UnexpectedMessage(ExtensionType, MessageType),
    ClientNotFound(DownstreamId),
    ClientSenderNotFound(DownstreamId),
    PendingDeclareMiningJobNotFound(RequestId),
    UnsupportedProtocol,
    UnsupportedConnectionFlags,
    OneshotRecv(tokio::sync::oneshot::error::RecvError),
    InvalidConfig(String),
}

impl<Owner> From<JDSError<Owner>> for JDSErrorKind {
    fn from(value: JDSError<Owner>) -> Self {
        value.kind
    }
}

impl<Owner> JDSError<Owner> {
    /// Constructs a [`JDSError`] with [`Action::Log`].
    pub fn log<E: Into<JDSErrorKind>>(kind: E) -> Self {
        Self {
            kind: kind.into(),
            action: Action::Log,
            _owner: PhantomData,
        }
    }
}

impl From<std::io::Error> for JDSErrorKind {
    fn from(e: std::io::Error) -> Self {
        JDSErrorKind::Io(e)
    }
}

impl From<async_channel::RecvError> for JDSErrorKind {
    fn from(e: async_channel::RecvError) -> Self {
        JDSErrorKind::ChannelRecv(e)
    }
}

impl From<binary_sv2::Error> for JDSErrorKind {
    fn from(e: binary_sv2::Error) -> Self {
        JDSErrorKind::BinarySv2(e)
    }
}

impl From<codec_sv2::Error> for JDSErrorKind {
    fn from(e: codec_sv2::Error) -> Self {
        JDSErrorKind::Codec(e)
    }
}

impl From<framing_sv2::Error> for JDSErrorKind {
    fn from(e: framing_sv2::Error) -> Self {
        JDSErrorKind::Framing(e)
    }
}

impl From<noise_sv2::Error> for JDSErrorKind {
    fn from(e: noise_sv2::Error) -> Self {
        JDSErrorKind::Noise(e)
    }
}

impl From<ParserError> for JDSErrorKind {
    fn from(e: ParserError) -> Self {
        JDSErrorKind::Parser(e)
    }
}

impl From<tokio::sync::oneshot::error::RecvError> for JDSErrorKind {
    fn from(e: tokio::sync::oneshot::error::RecvError) -> Self {
        JDSErrorKind::OneshotRecv(e)
    }
}

impl<T> From<async_channel::SendError<T>> for JDSErrorKind
where
    T: std::marker::Send + Debug + 'static,
{
    fn from(e: async_channel::SendError<T>) -> Self {
        JDSErrorKind::ChannelSend(Box::new(e))
    }
}

impl<Owner> HandlerErrorType for JDSError<Owner> {
    fn parse_error(error: ParserError) -> Self {
        Self {
            kind: JDSErrorKind::Parser(error),
            action: Action::Log,
            _owner: PhantomData,
        }
    }

    fn unexpected_message(extension_type: ExtensionType, message_type: MessageType) -> Self {
        Self {
            kind: JDSErrorKind::UnexpectedMessage(extension_type, message_type),
            action: Action::Log,
            _owner: PhantomData,
        }
    }
}
