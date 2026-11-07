//! Ordered status persistence, kept off the terminal event loop.
use super::app::Work;
use skyhook::{agent::SessionHandle, identity::AgentId};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub struct StatusLog(mpsc::UnboundedSender<Request>);
enum Request {
    Record {
        session: Option<SessionHandle>,
        agent: AgentId,
        message: String,
    },
    Flush(oneshot::Sender<()>),
}
#[derive(Clone)]
pub struct StatusSender {
    log: StatusLog,
    session: Option<SessionHandle>,
    agent: AgentId,
}
impl StatusLog {
    pub fn new(work: mpsc::UnboundedSender<Work>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                match request {
                    Request::Record {
                        session,
                        agent,
                        message,
                    } => {
                        if let Some(session) = session {
                            // A session that no longer runs journals nothing: its
                            // notices are only shown, as is the one a failed
                            // journal refuses, which the session shows already.
                            let unsaved = if session.phase().runs() {
                                match session.record_status(agent.clone(), message.clone()).await {
                                    Ok(()) => None,
                                    Err(error) if error.journal_failed() => Some(message),
                                    Err(error) => Some(format!(
                                        "{message}\nCould not save this status: {error}"
                                    )),
                                }
                            } else {
                                Some(message)
                            };
                            if let Some(message) = unsaved {
                                let session = session.id();
                                let _ = work.send(Work::Unsaved {
                                    session,
                                    agent,
                                    message,
                                });
                            }
                        } else {
                            // Before the first message, notices are UI-only.
                            let _ = work.send(Work::LocalStatus { agent, message });
                        }
                    }
                    Request::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Self(tx)
    }
    pub fn sender(&self, session: Option<&SessionHandle>, agent: &AgentId) -> StatusSender {
        StatusSender {
            log: self.clone(),
            session: session.cloned(),
            agent: agent.clone(),
        }
    }
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.0.send(Request::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}
impl StatusSender {
    pub fn send(&self, message: impl Into<String>) {
        let message = message.into();
        if !message.trim().is_empty() {
            let _ = self.log.0.send(Request::Record {
                session: self.session.clone(),
                agent: self.agent.clone(),
                message,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl StatusLog {
        /// A log that answers nothing, flushes included, until `release` is sent.
        pub(in crate::tui) fn held() -> (Self, oneshot::Sender<()>) {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let (release, released) = oneshot::channel();
            tokio::spawn(async move {
                let _ = released.await;
                while let Some(request) = rx.recv().await {
                    if let Request::Flush(done) = request {
                        let _ = done.send(());
                    }
                }
            });
            (Self(tx), release)
        }
    }
}
