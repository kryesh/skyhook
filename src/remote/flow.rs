//! A fixed byte-bounded window shared by relayed SSH and tool payload streams.
use std::{io, sync::Arc};

use tokio::io::{AsyncRead, AsyncReadExt as _};

pub(crate) const CHUNK_BYTES: usize = crate::tool::output::OUTPUT_CHUNK_BYTES;
pub(crate) const WINDOW: usize = 16;
/// A window of chunks and the end that follows them.
pub(crate) const QUEUE: usize = WINDOW + 1;

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

/// Send `reader` in chunks, each spending one of `credits` when given, then
/// `None` at its end, which spends none. Closed credits mean the receiver has
/// gone, so sending stops without error.
pub(crate) async fn pump<E: From<io::Error>, Sent: Future<Output = Result<(), E>>>(
    mut reader: impl AsyncRead + Unpin,
    credits: Option<&Credits>,
    mut send: impl FnMut(Option<Vec<u8>>) -> Sent,
) -> Result<(), E> {
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return send(None).await;
        }
        if let Some(credits) = credits
            && credits.take().await.is_err()
        {
            return Ok(());
        }
        send(Some(buffer[..read].to_vec())).await?;
    }
}
