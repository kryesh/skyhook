//! Reading fields kept in files on a job's target: a stream the target wrote there,
//! or a file a read named. The session holds only their first bytes, so reading the
//! rest is a read on that target, made off the session's readers by a call that
//! reaches it: a target answers when it does, and a session's reads never wait for
//! one. A file that cannot be reached or read fails its read as saved output that is
//! no longer available.
use super::*;
use crate::session::TargetFile;
use crate::tool::target_tools::{ExecutionTarget, read_file};
use futures_util::future::BoxFuture;
use skyhook_tools::{diagnostic::Cause, output::CaptureFile, target::ExecutionLocation};
use skyhook_worker::op::{self, TargetPath};
use std::{pin::Pin, task::Poll};
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// How a call reaches the files a job's target holds of its saved output.
pub(crate) trait Reach: Send + Sync {
    /// `file` of a job that ran at `ran`, to read where it is: on the machine the
    /// route it was kept along still reaches, and no other. A stream its target wrote
    /// is the session's own saved output, read back unasked; a file a read named is
    /// reached as a read of it is, asking what that asks.
    fn reach<'a>(
        &'a self,
        ran: &'a ExecutionLocation,
        file: TargetFile,
    ) -> BoxFuture<'a, Result<HeldFile, ToolError>>;
}

/// The target a call ran on and where there: what its own output keeps in files
/// there, it reads there.
#[derive(Clone)]
pub(crate) struct TargetFiles {
    target: ExecutionTarget,
    location: ExecutionLocation,
}

impl TargetFiles {
    pub(crate) fn new(target: ExecutionTarget, location: ExecutionLocation) -> Self {
        Self { target, location }
    }
}

impl Reach for TargetFiles {
    fn reach<'a>(
        &'a self,
        _: &'a ExecutionLocation,
        TargetFile { file, .. }: TargetFile,
    ) -> BoxFuture<'a, Result<HeldFile, ToolError>> {
        let (target, location) = (self.target.clone(), self.location.clone());
        let path = file.path().clone();
        Box::pin(std::future::ready(Ok(HeldFile {
            target,
            location,
            path,
            file,
        })))
    }
}

/// A file on a job's target that holds a field of its output, reached to be read
/// there: at `path`, as its target spells it for the read.
pub(crate) struct HeldFile {
    pub(crate) target: ExecutionTarget,
    pub(crate) location: ExecutionLocation,
    pub(crate) path: TargetPath,
    pub(crate) file: CaptureFile,
}

/// How reading `field` fails once the bytes saved for it cannot be read.
pub(super) fn unavailable(field: &FieldPointer) -> ToolError {
    ToolError::failed(format!("saved output for {field} is no longer available"))
}

/// How reading `field` fails with what kept its file from being reached or opened:
/// as its reader was stopped or refused, and else as unavailable.
fn lost(field: &FieldPointer) -> impl Fn(ToolError) -> ToolError + '_ {
    |error| match error.facts().cause {
        Cause::Cancelled | Cause::Interrupted | Cause::Denied(_) | Cause::Unavailable { .. } => {
            error
        }
        _ => unavailable(field),
    }
}

/// The whole text of each of `held`, fields of the output of a job that ran at
/// `ran`, read where its target keeps them: what a read that needs them whole
/// installs before it reads on. A read of at most `limit` bytes of JSON fails as
/// one over it does once a field alone is longer.
pub(super) async fn texts(
    reach: Option<&dyn Reach>,
    ran: &ExecutionLocation,
    held: Held,
    limit: Option<usize>,
    cancellation: &crate::job::CancellationToken,
) -> Result<Vec<(FieldPointer, String)>, ToolError> {
    let read = async {
        let mut texts = Vec::with_capacity(held.len());
        for (field, file) in held {
            let reach = reach.ok_or_else(|| unavailable(&field))?;
            let file = reach.reach(ran, file).await.map_err(lost(&field))?;
            let text = file.text(&field, limit.map_or(u64::MAX, |limit| limit as u64));
            texts.push((field.clone(), text.await?));
        }
        Ok(texts)
    };
    tokio::select! {
        texts = read => texts,
        () = cancellation.cancelled() => Err(ToolError::cancelled()),
    }
}

/// One page of `file`, which holds the field `selection` selects of the output of a
/// job that ran at `ran`, read where it is.
pub(super) async fn page(
    reach: Option<&dyn Reach>,
    ran: &ExecutionLocation,
    file: TargetFile,
    selection: Selection,
    terminal: bool,
    cancellation: &crate::job::CancellationToken,
) -> Result<LinePage, ToolError> {
    let field = selection.field.clone();
    let reach = reach.ok_or_else(|| unavailable(&field))?;
    let reaching = tokio::select! {
        file = reach.reach(ran, file) => file,
        () = cancellation.cancelled() => return Err(ToolError::cancelled()),
    };
    let file = reaching.map_err(lost(&field))?;
    file.page(selection, terminal, cancellation).await
}

impl HeldFile {
    /// The file's bytes from `from` on. What a target wrote is read no further than
    /// it recorded, and fails once the file holds less; a file a read named is read
    /// as it is now.
    async fn open(
        &self,
        from: op::ReadFrom,
    ) -> Result<impl AsyncRead + Send + Unpin + 'static, ToolError> {
        let read = op::Read {
            path: self.path.clone(),
            workspace: self.location.workspace.clone(),
            mode: op::ReadMode::File,
            from,
        };
        let (length, offset, bytes) = read_file(&self.target, read, &self.location).await?;
        let recorded = match &self.file {
            CaptureFile::Written(stream) if length < stream.extent.bytes => {
                return Err(ToolError::failed("the file is shorter than was saved"));
            }
            CaptureFile::Written(stream) => stream.extent.bytes,
            CaptureFile::Named { .. } => u64::MAX,
        };
        Ok(bytes.take(recorded.saturating_sub(offset)))
    }

    /// All the text of the file, which holds `field`, within `limit` bytes.
    async fn text(&self, field: &FieldPointer, limit: u64) -> Result<String, ToolError> {
        let over = || query::too_large(usize::try_from(limit).unwrap_or(usize::MAX));
        if matches!(&self.file, CaptureFile::Written(stream) if stream.extent.bytes > limit) {
            return Err(over());
        }
        let bytes = self.open(op::ReadFrom::Start).await.map_err(lost(field))?;
        let (mut bytes, mut text) = (bytes.take(limit.saturating_add(1)), Vec::new());
        let read = bytes.read_to_end(&mut text).await;
        read.map_err(|_| unavailable(field))?;
        if text.len() as u64 > limit {
            return Err(over());
        }
        String::from_utf8(text).map_err(|_| reader::not_utf8())
    }

    /// One page of the file, read where it is from the first line the page reads:
    /// nothing before it is sent. The read stops once `cancellation` is cancelled
    /// or the page's caller is gone.
    async fn page(
        &self,
        selection: Selection,
        terminal: bool,
        cancellation: &crate::job::CancellationToken,
    ) -> Result<LinePage, ToolError> {
        let field = selection.field.clone();
        let stopped = cancellation.child_token();
        let _abandoned = stopped.clone().drop_guard();
        let from = op::ReadFrom::Line(reader::first_line(&selection) as u64);
        let bytes = tokio::select! {
            bytes = self.open(from) => bytes.map_err(lost(&field))?,
            () = stopped.cancelled() => return Err(ToolError::cancelled()),
        };
        let bytes = Stoppable {
            bytes,
            waiting: Box::pin(stopped.clone().cancelled_owned()),
            stopped: stopped.clone(),
        };
        let lines = self.file.extent().lines();
        let runtime = tokio::runtime::Handle::current();
        // Paging reads as it goes, which a target's stream is bridged to on a thread
        // that may block.
        let paged = tokio::task::spawn_blocking(move || {
            let bytes = tokio_util::io::SyncIoBridge::new_with_handle(bytes, runtime);
            reader::file_page(bytes, lines, &selection, terminal, &stopped)
        });
        let paged = paged.await.map_err(ToolError::failed)?;
        check_cancelled(cancellation)?;
        // Only its target's stream fails a page with an I/O failure.
        paged.map_err(|error| match error.facts().cause {
            Cause::Io(_) => unavailable(&field),
            _ => error,
        })
    }
}

/// A target's bytes, which fail to read once their reader is stopped.
struct Stoppable<R> {
    bytes: R,
    stopped: crate::job::CancellationToken,
    waiting: Pin<Box<tokio_util::sync::WaitForCancellationFutureOwned>>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Stoppable<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.stopped.is_cancelled() || self.waiting.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(std::io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.bytes).poll_read(context, buffer)
    }
}
