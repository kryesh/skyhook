//! Ordered status persistence, kept off the terminal event loop.
use super::app::Work;
use skyhook::{agent::SessionHandle, identity::AgentId};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub struct StatusLog(mpsc::UnboundedSender<Request>);
enum Request {
    Record {
        session: SessionHandle,
        agent: AgentId,
        message: String,
    },
    Flush(oneshot::Sender<()>),
}
#[derive(Clone)]
pub struct StatusSender {
    log: StatusLog,
    session: SessionHandle,
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
                        if let Err(error) =
                            session.record_status(agent.clone(), message.clone()).await
                        {
                            let _ = work.send(Work::StatusFailed {
                                session: session.id(),
                                agent,
                                message: format!("{message}\nCould not save this status: {error}"),
                            });
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
    pub fn sender(&self, session: &SessionHandle, agent: &AgentId) -> StatusSender {
        StatusSender {
            log: self.clone(),
            session: session.clone(),
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
