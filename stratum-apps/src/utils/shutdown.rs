//! Process shutdown signalling.

use std::io;

use tokio::signal::unix::{Signal, SignalKind, signal};

/// Listener for the signals that ask the process to stop.
pub struct ShutdownSignal {
    sigint: Signal,
    sigterm: Signal,
}

impl ShutdownSignal {
    /// Starts listening for `SIGINT` and `SIGTERM`.
    ///
    /// Listening begins here rather than at the first [`ShutdownSignal::wait`] call, so a caller
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

    /// Waits for the next `SIGINT` or `SIGTERM`, returning its name and the exit status
    /// conventionally associated with it.
    ///
    /// `SIGINT` comes from an interactive terminal (Ctrl+C), `SIGTERM` from service managers and
    /// container runtimes. The listeners stay alive across calls, so a caller that has already
    /// begun shutting down can wait again to learn that the operator is no longer willing to
    /// wait for the graceful path, and exit with the accompanying status.
    ///
    /// That status is `128 + the signal number` (`SIGINT` is 2, `SIGTERM` is 15), which is what a
    /// shell or a container runtime reports when a signal terminates a process. Exiting with it
    /// is not the same outcome as being terminated by the signal, which a supervisor can tell
    /// apart, but it is the number tooling already reads as "stopped by an operator".
    ///
    /// Signals are coalesced, so two that arrive close enough together can be reported as one.
    pub async fn wait(&mut self) -> (&'static str, i32) {
        tokio::select! {
            _ = self.sigint.recv() => ("SIGINT", 130), // 128 + 2
            _ = self.sigterm.recv() => ("SIGTERM", 143), // 128 + 15
        }
    }
}
