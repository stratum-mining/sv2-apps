//! Process shutdown signalling.

use std::{fmt, io};

use tokio::signal::unix::{Signal, SignalKind, signal};

/// A signal asking the process to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    /// `SIGINT`, sent by an interactive terminal on Ctrl+C.
    Interrupt,
    /// `SIGTERM`, sent by service managers and container runtimes.
    Terminate,
}

impl StopSignal {
    /// The exit status conventionally associated with this signal.
    ///
    /// That status is `128 + the signal number` (`SIGINT` is 2, `SIGTERM` is 15), which is what a
    /// shell or a container runtime reports when a signal terminates a process. Exiting with it
    /// is not the same outcome as being terminated by the signal, which a supervisor can tell
    /// apart, but it is the number tooling already reads as "stopped by an operator".
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Interrupt => 130, // 128 + 2
            Self::Terminate => 143, // 128 + 15
        }
    }
}

impl fmt::Display for StopSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        })
    }
}

/// Listener for the signals that ask the process to stop.
pub struct ShutdownListener {
    sigint: Signal,
    sigterm: Signal,
}

impl ShutdownListener {
    /// Starts listening for `SIGINT` and `SIGTERM`.
    ///
    /// Listening begins here rather than at the first [`ShutdownListener::wait`] call, so a caller
    /// that builds the listener before it starts serving cannot miss a signal that arrives
    /// during startup.
    ///
    /// An error means the process has no way to observe either signal, so callers treat it as
    /// fatal rather than running on without a shutdown path.
    pub fn new() -> Result<Self, io::Error> {
        Ok(Self {
            sigint: signal(SignalKind::interrupt())?,
            sigterm: signal(SignalKind::terminate())?,
        })
    }

    /// Waits for the next stop signal.
    ///
    /// The listeners stay alive across calls, so a caller that has already begun shutting down
    /// can wait again to learn that the operator is no longer willing to wait for the graceful
    /// path, and exit with [`StopSignal::exit_code`].
    ///
    /// Signals are coalesced, so two that arrive close enough together can be reported as one.
    pub async fn wait(&mut self) -> StopSignal {
        tokio::select! {
            _ = self.sigint.recv() => StopSignal::Interrupt,
            _ = self.sigterm.recv() => StopSignal::Terminate,
        }
    }
}
