//! Byte-stream ownership for the shim protocol, independent of child-process pipes.
use std::{
    future::Future as _,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
pub(crate) type Reader = Box<dyn AsyncRead + Unpin + Send>;
pub(crate) type Writer = Box<dyn AsyncWrite + Unpin + Send>;
pub(crate) struct Transport {
    pub input: Writer,
    pub output: Reader,
    pub owner: Box<dyn Send>,
}

pub(crate) struct ProcessOwner {
    pub cancellation: tokio_util::sync::CancellationToken,
    pub _config: super::ssh::SshConfig,
}
impl Drop for ProcessOwner {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(crate) struct ProcessReader {
    pub output: tokio::process::ChildStdout,
    pub completion: tokio::sync::oneshot::Receiver<Result<(), String>>,
    pub ended: bool,
}
impl AsyncRead for ProcessReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.ended || buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let before = buffer.filled().len();
        match Pin::new(&mut self.output).poll_read(cx, buffer) {
            Poll::Ready(Ok(())) if before == buffer.filled().len() => {
                match Pin::new(&mut self.completion).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        self.ended = true;
                        Poll::Ready(match result {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(error)) => Err(std::io::Error::other(error)),
                            Err(_) => Err(std::io::Error::other("SSH process supervisor stopped")),
                        })
                    }
                }
            }
            result => result,
        }
    }
}

pub(crate) struct RelayedReader<R> {
    pub output: R,
    pub failure: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}
impl<R: AsyncRead + Unpin> AsyncRead for RelayedReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        match Pin::new(&mut self.output).poll_read(cx, buffer) {
            Poll::Ready(Ok(())) if before == buffer.filled().len() && buffer.remaining() > 0 => {
                Poll::Ready(
                    self.failure
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .map_or(Ok(()), |error| Err(std::io::Error::other(error.clone()))),
                )
            }
            result => result,
        }
    }
}
