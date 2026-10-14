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
            "failed to initialize bitcoin_core_sv2 {} for v{}: {}",
            self.protocol.as_str(),
            self.version.as_major(),
            self.details
        )
    }
}

impl std::error::Error for BitcoinCoreSv2Error {}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::CancellationToken;

    #[tokio::test]
    async fn ipc_factories_report_unsupported_platform() {
        for version in [BitcoinCoreVersion::V30X, BitcoinCoreVersion::V31X] {
            let (_, incoming) = async_channel::unbounded();
            let (outgoing, _) = async_channel::unbounded();
            let error = template_distribution_protocol::new(
                version,
                "node.sock",
                0,
                1,
                incoming,
                outgoing,
                CancellationToken::new(),
            )
            .await
            .err()
            .expect("Windows TDP initialization must return an error");
            assert!(error.to_string().contains("not supported on Windows"));

            let (_, incoming) = async_channel::unbounded();
            let (ready, _) = tokio::sync::oneshot::channel();
            let error = job_declaration_protocol::new(
                version,
                "node.sock",
                incoming,
                CancellationToken::new(),
                ready,
            )
            .await
            .err()
            .expect("Windows JDP initialization must return an error");
            assert!(error.to_string().contains("not supported on Windows"));
        }
    }
}
