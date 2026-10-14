//! Version-agnostic API for Bitcoin Core IPC integrations.

pub mod job_declaration_protocol;
pub mod template_distribution_protocol;

use std::fmt;

/// Supported Bitcoin Core IPC schema families.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitcoinCoreVersion {
    V30X,
    V31X,
}

impl BitcoinCoreVersion {
    pub const fn as_major(self) -> u8 {
        match self {
            Self::V30X => 30,
            Self::V31X => 31,
        }
    }
}

impl TryFrom<u8> for BitcoinCoreVersion {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            30 => Ok(Self::V30X),
            31 => Ok(Self::V31X),
            _ => Err(value),
        }
    }
}

/// Protocol family associated with a Bitcoin Core Sv2 runtime initialization error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitcoinCoreSv2Protocol {
    TDP,
    JDP,
}

impl BitcoinCoreSv2Protocol {
    const fn as_str(self) -> &'static str {
        match self {
            Self::TDP => "TDP",
            Self::JDP => "JDP",
        }
    }
}

/// Why a runtime constructor returned without a runtime.
///
/// This is reported by [`job_declaration_protocol::new`] and
/// [`template_distribution_protocol::new`] only. Once a runtime exists there is nothing left to
/// report this way: cancelling it ends `run()`, which returns nothing.
#[derive(Debug)]
pub enum BitcoinCoreSv2Error {
    /// The cancellation token the caller passed in fired during construction, so no runtime was
    /// built. It covers a token already cancelled when the constructor was called as much as one
    /// that fires partway through bootstrap.
    ///
    /// This is the expected outcome of shutting down while starting up, not a failure: the caller
    /// asked for it. Callers tell it apart with [`BitcoinCoreSv2Error::is_cancelled`], so a clean
    /// shutdown is not reported as an unreachable node.
    Cancelled {
        version: BitcoinCoreVersion,
        protocol: BitcoinCoreSv2Protocol,
    },
    /// Initialization failed. `details` describes the version-specific cause, for logging.
    Initialization {
        version: BitcoinCoreVersion,
        protocol: BitcoinCoreSv2Protocol,
        details: String,
    },
}

impl BitcoinCoreSv2Error {
    /// Whether construction stopped because the caller's cancellation token fired.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }

    pub(crate) fn initialization<E>(
        version: BitcoinCoreVersion,
        protocol: BitcoinCoreSv2Protocol,
        error: E,
    ) -> Self
    where
        E: fmt::Debug,
    {
        Self::Initialization {
            version,
            protocol,
            details: format!("{error:?}"),
        }
    }
}

impl fmt::Display for BitcoinCoreSv2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled { version, protocol } => write!(
                f,
                "bitcoin_core_sv2 {} for v{} bootstrap gave way to cancellation",
                protocol.as_str(),
                version.as_major()
            ),
            Self::Initialization {
                version,
                protocol,
                details,
            } => write!(
                f,
                "failed to initialize bitcoin_core_sv2 {} for v{}: {}",
                protocol.as_str(),
                version.as_major(),
                details
            ),
        }
    }
}

impl std::error::Error for BitcoinCoreSv2Error {}
