//! The termination signals an application ends on.
use tokio::signal::unix::{Signal, SignalKind, signal};

#[derive(Debug, thiserror::Error)]
#[error("could not watch for termination signals: {0}")]
pub struct Unwatched(#[source] std::io::Error);

/// The signals that end a host: SIGTERM, SIGHUP and SIGINT.
pub struct Signals([Signal; 3]);

impl Signals {
    const WATCHED: [(SignalKind, &str); 3] = [
        (SignalKind::terminate(), "SIGTERM"),
        (SignalKind::hangup(), "SIGHUP"),
        (SignalKind::interrupt(), "SIGINT"),
    ];

    /// Watch for them from now on.
    pub fn watch() -> Result<Self, Unwatched> {
        let [terminate, hangup, interrupt] = Self::WATCHED.map(|(kind, _)| signal(kind));
        let watched = || Ok(Self([terminate?, hangup?, interrupt?]));
        watched().map_err(Unwatched)
    }

    /// The next one to arrive, by name.
    pub async fn recv(&mut self) -> &'static str {
        let [terminate, hangup, interrupt] = &mut self.0;
        let index = tokio::select! {
            _ = terminate.recv() => 0,
            _ = hangup.recv() => 1,
            _ = interrupt.recv() => 2,
        };
        Self::WATCHED[index].1
    }
}
