//! Byte-stream ownership for the shim protocol, independent of child-process pipes.
use std::{
    future::Future as _,
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::oneshot,
};

use super::error::RemoteError;

pub(crate) type Reader = Box<dyn AsyncRead + Unpin + Send>;
pub(crate) type Writer = Box<dyn AsyncWrite + Unpin + Send>;
pub(crate) struct Transport {
    pub input: Writer,
    pub output: Reader,
    pub owner: Box<dyn Send>,
}

/// Reads `output`, and at its end the result of whatever produced it: a failed
/// producer ends the stream with its error rather than a clean end of file.
pub(crate) struct ReportingReader<R> {
    output: R,
    result: Option<oneshot::Receiver<Result<(), RemoteError>>>,
}

impl<R> ReportingReader<R> {
    pub(crate) fn new(output: R, result: oneshot::Receiver<Result<(), RemoteError>>) -> Self {
        Self {
            output,
            result: Some(result),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ReportingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let before = buffer.filled().len();
        ready!(Pin::new(&mut self.output).poll_read(cx, buffer))?;
        if before != buffer.filled().len() {
            return Poll::Ready(Ok(()));
        }
        let Some(result) = &mut self.result else {
            return Poll::Ready(Ok(()));
        };
        let result = ready!(Pin::new(result).poll(cx));
        self.result = None;
        Poll::Ready(match result {
            Ok(result) => result.map_err(io::Error::other),
            // The producer stopped without reporting: the stream was cut short.
            Err(_) => Err(io::ErrorKind::UnexpectedEof.into()),
        })
    }
}
