//! How the session store fails.

use std::sync::Arc;
use thiserror::Error;

use super::{BlobError, RecordSeq, ReplayError, RequestSeq, SessionPhase};
use crate::db::DbError;
use crate::identity::SessionId;

/// What a session stopped by its journal's failure reports.
pub(crate) const JOURNAL_FAILED: &str = "Skyhook could not write the session journal and stopped the session (a Skyhook bug or a storage problem)";

#[derive(Debug, Error)]
pub enum SessionError {
    /// Committed, or it may have: the database's report when the commit itself
    /// failed.
    #[error("the journal could not confirm an accepted append")]
    Unconfirmed(#[source] Option<DbError>),
    #[error("{JOURNAL_FAILED}: {0}")]
    JournalFailed(JournalFailure),
    #[error("the session is {0}")]
    Refused(SessionPhase),
    #[error("cannot reconstruct model request at sequence {sequence}: {reason}")]
    ModelRequestReplay {
        sequence: RequestSeq,
        reason: ReplayError,
    },
    #[error("session writer was lost before accepting the append")]
    WriterLost,
    #[error("session {0} was not found")]
    NotFound(SessionId),
    #[error("session I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Database(DbError),
    #[error("session identifier generation failed: {0}")]
    Random(#[from] getrandom::Error),
    #[error("session {0} is already open")]
    AlreadyOpen(SessionId),
    #[error("unsupported session version {0}")]
    UnsupportedVersion(i64),
    #[error("event belongs to another session")]
    WrongSession,
    #[error("journal entry {sequence} changes a job's phase illegally")]
    IllegalTransition { sequence: RecordSeq },
    #[error("blob `{digest}` {reason}")]
    Blob {
        digest: skyhook_media::BlobDigest,
        reason: BlobError,
    },
}

/// The first writer failure, which stopped the session: every later write
/// reports it.
#[derive(Clone, Debug)]
pub struct JournalFailure(pub(super) Arc<SessionError>);

impl JournalFailure {
    pub fn cause(&self) -> &SessionError {
        &self.0
    }
}

/// The same failure, not merely an equal one.
impl PartialEq for JournalFailure {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Display for JournalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<DbError> for SessionError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::Unsupported(version) => Self::UnsupportedVersion(version),
            error => Self::Database(error),
        }
    }
}
