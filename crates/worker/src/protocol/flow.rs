//! A fixed byte-bounded window shared by every call's streamed events and input.
use std::sync::Arc;

pub const WINDOW: usize = 64;
/// A window of chunks and the end that follows them.
pub const QUEUE: usize = WINDOW + 1;

#[derive(Clone)]
pub struct Credits(Arc<tokio::sync::Semaphore>);

impl Default for Credits {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(WINDOW)))
    }
}

impl Credits {
    pub async fn take(&self) {
        let credit = self.0.acquire().await;
        credit.expect("credits are never closed").forget();
    }

    /// Return one credit taken; false when none was.
    #[must_use]
    pub fn acknowledge(&self) -> bool {
        let owed = self.0.available_permits() < WINDOW;
        if owed {
            self.0.add_permits(1);
        }
        owed
    }
}
