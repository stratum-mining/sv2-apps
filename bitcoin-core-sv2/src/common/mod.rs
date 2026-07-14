//! Version-agnostic API for Bitcoin Core IPC integrations.

pub mod job_declaration_protocol;
pub mod template_distribution_protocol;

use std::fmt;

/// Supported Bitcoin Core IPC schema families.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitcoinCoreVersion {
    V30X,
    V31X,
    Master,
}

impl BitcoinCoreVersion {
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::V30X => "30",
            Self::V31X => "31",
            Self::Master => "master",
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

/// Error returned when selecting and initializing a versioned Bitcoin Core IPC runtime fails.
#[derive(Debug)]
pub struct BitcoinCoreSv2Error {
    version: BitcoinCoreVersion,
    protocol: BitcoinCoreSv2Protocol,
    details: String,
}

impl BitcoinCoreSv2Error {
    pub(crate) fn from_debug<E>(
        version: BitcoinCoreVersion,
        protocol: BitcoinCoreSv2Protocol,
        error: E,
    ) -> Self
    where
        E: fmt::Debug,
    {
        Self {
            version,
            protocol,
            details: format!("{error:?}"),
        }
    }
}

impl fmt::Display for BitcoinCoreSv2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to initialize bitcoin_core_sv2 {} for {}: {}",
            self.protocol.as_str(),
            self.version.as_label(),
            self.details
        )
    }
}

impl std::error::Error for BitcoinCoreSv2Error {}
