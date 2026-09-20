//! A fixed byte-bounded window shared by relayed SSH and tool payload streams.
use std::{io, sync::Arc};

pub(crate) const CHUNK_BYTES: usize = crate::tool::output::OUTPUT_CHUNK_BYTES;
pub(crate) const WINDOW: usize = 16;

#[derive(Clone)]
pub(crate) struct Credits(Arc<tokio::sync::Semaphore>);

impl Default for Credits {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(WINDOW)))
    }
}

impl Credits {
    pub(crate) fn reserve(&self) -> io::Result<tokio::sync::OwnedSemaphorePermit> {
        self.0.clone().try_acquire_owned().map_err(io::Error::other)
    }

    pub(crate) async fn take(&self) -> io::Result<()> {
        self.0.acquire().await.map_err(io::Error::other)?.forget();
        Ok(())
    }

    pub(crate) fn acknowledge(&self) -> io::Result<()> {
        if self.0.available_permits() >= WINDOW {
            return Err(io::Error::other("invalid stream credit"));
        }
        self.0.add_permits(1);
        Ok(())
    }

    pub(crate) fn close(&self) {
        self.0.close();
    }
}
