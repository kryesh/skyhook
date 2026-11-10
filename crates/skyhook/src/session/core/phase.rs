//! Where a session is in its life, and what each phase admits.
use super::{JournalFailure, SessionError};

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
    /// nothing more is journaled, and every later write reports the failure. Its
    /// state is replaced by what reopening shows, so an observation that folds
    /// this phase no longer follows the session: observe it again.
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
    /// Start new work: create a job, launch an agent, begin a turn, journal a model
    /// request or an attempt at one, or take input.
    Start,
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
            (Running, Start | Journal) => Running,
            // Closing leaves an interruption suspended, as the journal leaves it for
            // the next process: settling it would let a released owner claim it.
            (Viewing, Start | Journal)
            | (Running, Activate)
            | (ShuttingDown | Closed, Activate | Start)
            | (Closed, Journal)
            | (Failed(_), Activate | Start | Journal | BeginShutdown | Close) => {
                return Err(self.refusal());
            }
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
    pub fn failure(&self) -> Option<&JournalFailure> {
        match self {
            Self::Failed(failure) => Some(failure),
            Self::Viewing | Self::Running | Self::ShuttingDown | Self::Closed => None,
        }
    }

    /// Whether its work may still run and journal what it does, as that of a
    /// viewed, failed or closed session does not.
    pub fn runs(&self) -> bool {
        matches!(self, Self::Running | Self::ShuttingDown)
    }

    /// When the work a session that no longer runs left going stopped, as it is
    /// read: at its last activity.
    pub(crate) fn stopped_at(&self, last_activity: Option<i64>) -> Option<i64> {
        (!self.runs()).then(|| last_activity.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Plan, SessionEvent, SessionPhase::*, SessionStore};

    /// Each phase admits its commands and moves where they take it: a viewed
    /// session only activates or shuts down, which closes it; a running one does
    /// all but activate; one shutting down only records how its running work
    /// ends; a closed one only shuts down or closes, staying closed. The store
    /// journals what is admitted, except on closing, and nothing it refuses, which
    /// leaves the phase as it was. Only a running session that goes on running
    /// dates what it journals now, the rest at their last activity.
    #[tokio::test]
    async fn the_store_journals_what_its_phase_admits_and_nothing_it_refuses() {
        use Command::*;
        let crate::tests::TestRuntime {
            root, store, agent, ..
        } = crate::tests::TestRuntime::on_disk().await;
        let (sessions, id) = (root.path().join("sessions"), store.id());
        drop(store);
        // Each chain starts from the session reopened to view it.
        let chains: [&[(Command, Option<SessionPhase>)]; 2] = [
            &[
                (Start, None),
                (Activate, Some(Running)),
                (Activate, None),
                (Start, Some(Running)),
                (BeginShutdown, Some(ShuttingDown)),
                (Start, None),
                (Journal, Some(ShuttingDown)),
                (BeginShutdown, Some(ShuttingDown)),
                (Close, Some(Closed)),
                (Journal, None),
                (BeginShutdown, Some(Closed)),
                (Close, Some(Closed)),
            ],
            &[(BeginShutdown, Some(Closed))],
        ];
        for chain in chains {
            let store = SessionStore::open(&sessions, id, None).await.unwrap();
            for (command, expected) in chain.iter().cloned() {
                let phase = store.read(|state| state.phase().clone());
                let closes = expected == Some(Closed);
                let journaled = expected.is_some() && !closes;
                if journaled {
                    // Strictly later, so a record dated now would show.
                    crate::tests::past(store.records().last().unwrap().timestamp_millis).await;
                }
                let before = store.records().len();
                let status = SessionEvent::Status {
                    message: format!("{command:?}"),
                };
                // A refused command carries its entry, so a write would show.
                let entries = Vec::from_iter((!closes).then(|| (agent.clone(), status)));
                let plan = move |_: &_| Ok::<_, SessionError>(Plan::journal(entries));
                match (store.commit(command, plan).await, &expected) {
                    (Ok(()), Some(_)) => {}
                    (Err(SessionError::Refused(refused)), None) => {
                        assert_eq!(refused, phase, "{command:?}");
                    }
                    (committed, _) => panic!("{phase} {command:?}: {committed:?}"),
                }
                let records = store.records();
                assert_eq!(records.len(), before + usize::from(journaled));
                if journaled {
                    let (new, earlier) = records.split_last().unwrap();
                    let activity = earlier.iter().rev().find(|r| r.event.is_activity());
                    let dated_now = new.timestamp_millis != activity.unwrap().timestamp_millis;
                    let now = phase == Running && expected == Some(Running);
                    assert_eq!(dated_now, now, "{phase} {command:?}");
                }
                let next = expected.unwrap_or(phase);
                assert_eq!(store.read(|state| state.phase().clone()), next);
            }
        }
    }
}
