//! Where a session is in its life, and what each phase admits.
use super::{EventRecord, JournalFailure, SessionError};

/// Where an open session is in its life.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum SessionPhase {
    /// Reopened to read: nothing runs or is journaled until the first command
    /// that changes it activates it.
    Viewing,
    #[default]
    Running,
    /// Stopping its work: what runs may finish and record that, but nothing
    /// new starts or resumes.
    ShuttingDown,
    /// Stopped by a write its journal could not take, as a crash stops it:
    /// nothing more is journaled, and every later write reports the failure.
    Failed(JournalFailure),
    /// Its writer is closed and its lock released.
    Closed,
}

impl std::fmt::Display for SessionPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Viewing => "viewing",
            Self::Running => "running",
            Self::ShuttingDown => "shutting down",
            Self::Failed(_) => "failed",
            Self::Closed => "closed",
        })
    }
}

/// What a caller asks of the session, as its phase admits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// Settle what the process that last ran the session left open.
    Activate,
    CreateJob,
    LaunchAgent,
    BeginTurn,
    /// Journal a model request, for a turn or its compaction.
    Request,
    Submit,
    Continue,
    Send,
    /// Settle a child agent's job as its loop ends.
    AgentExited,
    /// Record what running work does.
    Journal,
    BeginShutdown,
    Close,
}

/// When an append's entries are dated.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Dated {
    /// When the writer accepts them.
    Now,
    /// At the session's newest activity entry, so they do not advance its last
    /// activity; entry times need not follow sequence order.
    LastActivity,
}

impl SessionPhase {
    /// The phase the session moves to on `command`, and when what it journals
    /// is dated, unless this phase refuses it. Only a running session that goes
    /// on running journals activity: what a closing one journals is not, as what
    /// a crash leaves is not, nor is what activating a viewed one settles for the
    /// process that stopped. A failed session refuses everything with its
    /// failure.
    pub(crate) fn admits(&self, command: Command) -> Result<(Self, Dated), SessionError> {
        use {Command::*, SessionPhase::*};
        let next = match (self, command) {
            (Viewing, Activate) => Running,
            // A viewed session never ran, so shutting it down stops nothing and
            // closes it; a closed one has nothing left to stop or close.
            (Viewing | Running | ShuttingDown | Closed, Close)
            | (Viewing | Closed, BeginShutdown) => Closed,
            (Running | ShuttingDown, BeginShutdown) | (ShuttingDown, Journal) => ShuttingDown,
            (
                Running,
                CreateJob | LaunchAgent | BeginTurn | Request | Submit | Continue | Send
                | AgentExited | Journal,
            ) => Running,
            // Closing leaves an interruption suspended, as the journal leaves it for
            // the next process: settling it would let a released owner claim it.
            (
                Viewing,
                CreateJob | LaunchAgent | BeginTurn | Request | Submit | Continue | Send
                | AgentExited | Journal,
            )
            | (Running, Activate)
            | (
                ShuttingDown | Closed,
                Activate | CreateJob | LaunchAgent | BeginTurn | Request | Submit | Continue | Send
                | AgentExited,
            )
            | (Closed, Journal)
            | (
                Failed(_),
                Activate | CreateJob | LaunchAgent | BeginTurn | Request | Submit | Continue | Send
                | AgentExited | Journal | BeginShutdown | Close,
            ) => return Err(self.refusal()),
        };
        let dated = match (self, &next) {
            (Running, Running) => Dated::Now,
            _ => Dated::LastActivity,
        };
        Ok((next, dated))
    }

    /// Why this phase refuses what it does not admit: its failure, once the
    /// journal failed.
    pub(crate) fn refusal(&self) -> SessionError {
        match self {
            Self::Failed(failure) => SessionError::JournalFailed(failure.clone()),
            phase => SessionError::Refused(phase.clone()),
        }
    }

    /// What every call reports once the journal failed: its failure.
    pub(crate) fn failed(&self) -> Result<(), SessionError> {
        self.failure().map_or(Ok(()), |_| Err(self.refusal()))
    }

    /// The failure that stopped the session, if its journal failed.
    #[must_use]
    pub fn failure(&self) -> Option<&JournalFailure> {
        match self {
            Self::Failed(failure) => Some(failure),
            Self::Viewing | Self::Running | Self::ShuttingDown | Self::Closed => None,
        }
    }

    /// Whether its work may still run and journal what it does, as that of a
    /// viewed, failed or closed session does not.
    #[must_use]
    pub fn runs(&self) -> bool {
        matches!(self, Self::Running | Self::ShuttingDown)
    }

    /// When the work a session that no longer runs left going stopped, as it is
    /// read: at its last activity in `records`.
    pub(crate) fn stopped_at<'a>(
        &self,
        records: impl IntoIterator<Item = &'a EventRecord, IntoIter: DoubleEndedIterator>,
    ) -> Option<i64> {
        (!self.runs()).then(|| EventRecord::last_activity(records).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Plan, SessionEvent, SessionPhase::*, SessionStore};

    /// A session in `phase`, on disk, with its root started.
    async fn in_phase(
        phase: &SessionPhase,
    ) -> (tempfile::TempDir, SessionStore, crate::identity::AgentId) {
        let (root, store, agent) = crate::session::tests::on_disk().await;
        match phase {
            Viewing => {
                let id = store.id();
                drop(store);
                let (store, _) = SessionStore::open(root.path(), id).await.unwrap();
                return (root, store, agent);
            }
            Running => {}
            ShuttingDown => drop(store.enter(Command::BeginShutdown).await),
            Closed => {
                store.close().await.unwrap();
                store.close().await.unwrap();
            }
            Failed(_) => unreachable!("a failure has its own test"),
        }
        (root, store, agent)
    }

    /// Each phase admits its commands and moves where they take it: a viewed
    /// session only activates or shuts down, which closes it; a running one does
    /// all but activate; one shutting down only records how its running work
    /// ends; a closed one only shuts down or closes, staying closed. Only a
    /// running session that goes on running dates what it journals now, the rest
    /// at their last activity. A refused command journals nothing and leaves the
    /// phase as it was.
    #[tokio::test]
    async fn each_phase_admits_its_commands_and_a_refusal_journals_nothing() {
        use Command::*;
        let commands = [
            Activate,
            CreateJob,
            LaunchAgent,
            BeginTurn,
            Request,
            Submit,
            Continue,
            Send,
            AgentExited,
            Journal,
            BeginShutdown,
        ];
        for phase in [Viewing, Running, ShuttingDown, Closed] {
            for command in commands {
                let (_root, store, agent) = in_phase(&phase).await;
                // Strictly later, so a record dated now would show.
                let last = store.records().last().unwrap().timestamp_millis;
                crate::tests::past(last).await;
                let (admitted, next) = match (&phase, command) {
                    (Viewing, Activate) | (Running, Activate) => (phase == Viewing, Running),
                    (Viewing | Closed, BeginShutdown) => (true, Closed),
                    (Running, BeginShutdown) => (true, ShuttingDown),
                    (Running, _) => (true, Running),
                    (ShuttingDown, Journal | BeginShutdown) => (true, ShuttingDown),
                    _ => (false, phase.clone()),
                };
                let next = if admitted { next } else { phase.clone() };
                let before = store.records().len();
                let status = SessionEvent::Status {
                    message: format!("{command:?}"),
                };
                // Closing journals nothing.
                let entries = Vec::from_iter((next != Closed).then(|| (agent.clone(), status)));
                let plan = move |_: &_| Ok::<_, SessionError>(Plan::journal(entries));
                match store.commit(command, plan).await {
                    Ok(()) => assert!(admitted, "{phase} admitted {command:?}"),
                    Err(SessionError::Refused(refused)) => {
                        assert!(!admitted && refused == phase, "{phase} refused {command:?}");
                    }
                    Err(error) => panic!("{phase} {command:?}: {error}"),
                }
                let records = store.records();
                let journaled = admitted && next != Closed;
                assert_eq!(records.len(), before + usize::from(journaled));
                if journaled {
                    let (new, earlier) = records.split_last().unwrap();
                    let activity = earlier.iter().rev().find(|r| r.event.is_activity());
                    let dated_now = new.timestamp_millis != activity.unwrap().timestamp_millis;
                    let now = phase == Running && next == Running;
                    assert_eq!(dated_now, now, "{phase} {command:?}");
                }
                assert_eq!(store.read(|state| state.phase.clone()), next);
            }
        }
    }
}
