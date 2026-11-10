//! What a failed call leaves of the directories it created.

use crate::error::OperationError;
use skyhook_worker::op::Directories;

/// `error` as it leaves the directories `created` for a destination it did not
/// replace.
pub fn left_by<O>(created: Directories, error: OperationError<O>) -> OperationError<O> {
    match created.left(Some(error.diagnostic().context.effects)) {
        Some(effects) => error.effects(effects),
        None => error,
    }
}
