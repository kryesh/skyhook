//! File snapshots, directory listings and images.
use crate::diagnostic::PartialContext;
use crate::output::ProducedOutput;
use crate::spec::TargetTool;
use crate::{
    error::CallError,
    spec::{Call, ended},
};
use skyhook_worker::diagnostic::{Effects, Failure, Fault, Operation, Subject};

use super::super::ImageSummary;
use crate::output::{AsyncOutput, FinishedOutput, TextCaptureField};
use crate::path::{PathAccess, PathArgument};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook_media::{Classifier, DecodeError, Finished, MAX_IMAGE_BYTES, Step};
use skyhook_worker::{op, op::PathKind};

/// The most bytes of a referenced file its capture keeps: more than any preview
/// shows.
const REFERENCED_HEAD_BYTES: u64 = 2 * op::CHUNK_BYTES as u64;

/// Capture a text file or list a directory, or attach an image. A text file past the
/// saved-field limit is referenced where it is instead of captured.
pub struct Read;

impl TargetTool for Read {
    const NAME: &'static str = "read";
    const DESCRIPTION: &'static str =
        "Capture a complete UTF-8 file or directory listing, or attach a supported image";
    type Input = ReadArgs;
    type Admitted = ReadArgs;
    type Op = op::Read;
    type Rendering = ReadArgs;

    fn result_schema() -> Option<serde_json::Value> {
        Some(crate::schema::result_schema::<ReadOutput>())
    }

    fn admit(input: ReadArgs) -> Result<ReadArgs, crate::error::AdmissionError> {
        Ok(input)
    }

    fn paths(args: &mut ReadArgs) -> Vec<PathArgument<'_>> {
        vec![PathArgument::new(
            &mut args.path,
            PathAccess::Read,
            PathKind::Existing,
        )]
    }

    fn op(args: &mut ReadArgs, call: &Call) -> Result<op::Read, CallError> {
        Ok(op::Read {
            path: args.path.clone(),
            workspace: call.execution_location().workspace.clone(),
            mode: op::ReadMode::Entry {
                counts: call.spills().inline,
            },
            from: op::ReadFrom::Start,
        })
    }

    fn rendering(args: ReadArgs) -> ReadArgs {
        args
    }

    async fn render<U: Into<CallError> + Send + 'static>(
        args: ReadArgs,
        mut running: op::Running<op::Read, U>,
        call: &Call,
    ) -> Result<ProducedOutput, CallError> {
        let path = args.path.clone();
        let mut file = Classifier::default();
        let mut text = TextCapture::default();
        let mut entries = DirectoryGroups::default();
        // All the file holds, as its target counted one too long to capture: text
        // that long is referenced, and read no further than the head a preview shows.
        let mut referenced = None;
        let reference = |capture: FinishedOutput, path: &op::TargetPath, extent| {
            let path = path.clone();
            capture.in_file(crate::output::CaptureFile::Named { path, extent })
        };
        loop {
            if let Some(extent) = referenced
                && text.full
            {
                // Dropping the read stops it: the rest stays where it is.
                drop(running);
                let capture = reference(text.finish(call, &path).await?, &path, extent);
                let content = String::new();
                return Ok(ProducedOutput::serialized(ReadOutput::File { content })?
                    .with_captures(vec![capture]));
            }
            let event = tokio::select! {
                event = running.event() => event,
                () = call.cancelled() => {
                    return Err(CallError::cancelled()
                        .operation(Operation::Read, Subject::path(&path))
                        .effects(Effects::OutputIncomplete));
                }
            };
            match event {
                None => break,
                Some(op::ReadEvent::Opened { counted, .. }) => {
                    referenced = counted;
                    let head = REFERENCED_HEAD_BYTES.min(call.spills().inline);
                    text.head = referenced.map(|_| head);
                }
                // Dropping the read stops it once nothing more of it could be shown.
                Some(op::ReadEvent::Bytes(chunk)) => {
                    match file
                        .push(&chunk)
                        .map_err(|error| unreadable(error, &path))?
                    {
                        Step::Pending => {}
                        Step::Text(chunk) => text.write(call, &path, &chunk).await?,
                        Step::Binary => return Err(unreadable(DecodeError::Unsupported, &path)),
                    }
                }
                Some(op::ReadEvent::Entries(listed)) => {
                    for entry in listed {
                        entries.push(entry);
                    }
                }
            }
        }
        let end = ended(running).await??;
        let shown = match end {
            op::ReadEnd::Directory => {
                entries.sort();
                return Ok(ProducedOutput::serialized(ReadOutput::Directory {
                    entries,
                })?);
            }
            op::ReadEnd::File { shown } => shown,
        };
        let image = match file.finish().await {
            Ok(Finished::Text(rest)) => {
                text.write(call, &path, &rest).await?;
                let capture = text.finish(call, &path).await?;
                let capture = match referenced {
                    Some(extent) => reference(capture, &path, extent),
                    None => capture,
                };
                return Ok(ProducedOutput::serialized(ReadOutput::File {
                    content: String::new(),
                })?
                .with_captures(vec![capture]));
            }
            Ok(Finished::Image(image)) => image,
            Ok(Finished::Binary) => return Err(unreadable(DecodeError::Unsupported, &path)),
            Err(error) => return Err(unreadable(error, &path)),
        };
        let reference = call
            .store_image(Some(shown), &image)
            .await
            .map_err(|error| error.operation(Operation::StoreImage, Subject::path(&path)))?;
        Ok(ProducedOutput::serialized(ReadOutput::Image {
            image: ImageSummary::from(&reference),
        })?
        .with_images(vec![reference]))
    }
}

/// How a file that is not text fails to be shown: an image too large to keep, or bytes
/// that are no image it can show.
fn unreadable(error: DecodeError, path: &op::TargetPath) -> CallError {
    match error {
        DecodeError::TooLarge => {
            let limit = MAX_IMAGE_BYTES;
            Failure::at(
                Operation::Read,
                Subject::path(path),
                Fault::TooLarge { limit },
            )
            .into()
        }
        DecodeError::Unsupported => {
            CallError::failed("file is neither UTF-8 text nor a supported image")
                .operation(Operation::Deserialize, Subject::path(path))
        }
        error => CallError::failed(error).operation(Operation::Deserialize, Subject::path(path)),
    }
}

/// A text file's capture, opened with its first text. That of a file referenced
/// where it is takes its first `head` bytes, and is `full` once it has them.
#[derive(Default)]
struct TextCapture {
    capture: Option<AsyncOutput>,
    written: u64,
    head: Option<u64>,
    full: bool,
}

impl TextCapture {
    async fn open(
        capture: Option<AsyncOutput>,
        call: &Call,
        path: &op::TargetPath,
    ) -> Result<AsyncOutput, CallError> {
        match capture {
            Some(capture) => Ok(capture),
            None => Ok(call
                .text_capture(TextCaptureField::Content)
                .await
                .map_err(|error| error.operation(Operation::CreateCapture, Subject::path(path)))?
                .open_async()),
        }
    }

    async fn write(
        &mut self,
        call: &Call,
        path: &op::TargetPath,
        text: &str,
    ) -> Result<(), CallError> {
        let text = match self.head {
            None => text,
            Some(head) => {
                let room = usize::try_from(head.saturating_sub(self.written));
                let kept = &text[..text.floor_char_boundary(room.unwrap_or(usize::MAX))];
                self.written += kept.len() as u64;
                self.full |= kept.len() < text.len() || self.written >= head;
                kept
            }
        };
        if text.is_empty() {
            return Ok(());
        }
        let capture = Self::open(self.capture.take(), call, path).await?;
        let capture = self.capture.insert(capture);
        let written = capture.write_text(text).await;
        written.map_err(|error| CallError::io(error).or(Self::at(Operation::WriteCapture, path)))
    }

    async fn finish(self, call: &Call, path: &op::TargetPath) -> Result<FinishedOutput, CallError> {
        let capture = Self::open(self.capture, call, path).await?;
        let finished = capture.finish().await;
        finished.map_err(|error| CallError::io(error).or(Self::at(Operation::FinishCapture, path)))
    }

    /// The file whose capture failed at `operation`.
    fn at(operation: Operation, path: &op::TargetPath) -> PartialContext {
        PartialContext::new(operation, Subject::path(path))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// File or directory path
    path: op::TargetPath,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadOutput {
    File { content: String },
    Directory { entries: DirectoryGroups },
    Image { image: ImageSummary },
}

#[serde_with::skip_serializing_none]
#[derive(Default, Serialize, JsonSchema)]
struct DirectoryGroups {
    files: Vec<DirectoryFile>,
    directories: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    symlinks: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    other: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
struct DirectoryFile {
    name: String,
    bytes: u64,
}

impl DirectoryGroups {
    /// Classifies by the entry's own metadata, so a symlink is never followed.
    fn push(&mut self, entry: op::Entry) {
        let op::Entry { name, kind } = entry;
        match kind {
            op::EntryKind::File { bytes } => self.files.push(DirectoryFile { name, bytes }),
            op::EntryKind::Directory => self.directories.push(name),
            op::EntryKind::Symlink => self.symlinks.push(name),
            op::EntryKind::Other => self.other.push(name),
        }
    }

    fn sort(&mut self) {
        self.files.sort_by(|left, right| left.name.cmp(&right.name));
        for names in [&mut self.directories, &mut self.symlinks, &mut self.other] {
            names.sort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::diagnostic::Cause;
    use skyhook_worker::op::IoKind;
    use tokio_util::sync::CancellationToken;

    /// Invalid UTF-8 and NUL bytes are not text, whichever chunk they arrive in,
    /// and a file that is neither text nor an image fails as one: one past the
    /// saved-field limit too, whose NUL byte lies beyond all that would be shown.
    #[tokio::test]
    async fn text_is_utf8_without_nul_bytes() {
        let workspace = crate::spec::tests::Workspace::new().spilling_past(128 * 1024);
        let long = "é".repeat(64 * 1024);
        let late = ["a".repeat(256 * 1024).as_bytes(), b"\0"].concat();
        for (name, bytes, text) in [
            ("text", long.as_bytes().to_vec(), Some(long.as_str())),
            ("invalid", b"valid\xff".to_vec(), None),
            ("nul-byte", b"valid\0".to_vec(), None),
            ("split", b"valid\xc3".to_vec(), None),
            ("late-nul-byte", late, None),
        ] {
            std::fs::write(workspace.path().join(name), bytes).unwrap();
            let read = workspace.run("read", json!({"path": name})).await;
            match text {
                Some(text) => assert_eq!(read.unwrap()["content"], text, "{name}"),
                None => assert_matches!(
                    read.unwrap_err().diagnostic().cause,
                    Cause::Message(message) if message.contains("neither UTF-8 text"),
                    "{name}"
                ),
            }
        }
    }

    #[tokio::test]
    async fn capture_failures_name_their_stage() {
        use crate::output::OutputContext;
        use crate::output::capture::tests::{CaptureStage, FailingCapture};
        use crate::spec::tests::{CapturedOutput, call, run_named};

        let root = tempfile::tempdir().unwrap();
        tokio::fs::write(root.path().join("source.txt"), "captured text")
            .await
            .unwrap();
        let local = skyhook_worker::local::Local::new(root.path().to_owned());
        for (stage, operation) in [
            (CaptureStage::Open, Operation::CreateCapture),
            (CaptureStage::Write, Operation::WriteCapture),
            (CaptureStage::Finish, Operation::FinishCapture),
        ] {
            let output = OutputContext::new(Arc::new(FailingCapture::new(
                Arc::new(CapturedOutput::default()),
                stage,
                0,
                || std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )));
            let call = call(root.path(), output.clone(), CancellationToken::new());
            let arguments = json!({"path":"source.txt"});
            let error = run_named(&local, "read", arguments, call)
                .await
                .unwrap_err();
            assert_eq!(error.diagnostic().context.operation, operation);
            assert_matches!(
                error.diagnostic().cause,
                Cause::Io(skyhook_worker::op::OsError {
                    kind: IoKind::PermissionDenied,
                    ..
                })
            );
            output.settle().await.unwrap();
        }
    }

    #[tokio::test]
    async fn directories_group_entries_by_their_own_metadata() {
        let workspace = crate::spec::tests::Workspace::new();
        let listing = workspace.path().join("listing");
        std::fs::create_dir_all(listing.join("directory")).unwrap();
        std::fs::write(listing.join("file"), b"abc").unwrap();
        std::os::unix::fs::symlink(listing.join("file"), listing.join("link")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(listing.join("socket")).unwrap();
        let read = async |path| workspace.run("read", json!({"path": path})).await.unwrap();
        assert_eq!(
            read("listing").await,
            json!({"kind":"directory", "entries":{
                "files":[{"name":"file", "bytes":3}], "directories":["directory"],
                "symlinks":["link"], "other":["socket"]
            }})
        );
        assert_eq!(
            read("listing/directory").await,
            json!({"kind":"directory", "entries":{"files":[], "directories":[]}})
        );
    }
}
