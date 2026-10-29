//! File snapshots, directory listings and images.
use crate::tool::ToolOptions;
use crate::tool::diagnostic::{Effects, Operation, Subject};
use crate::tool::invocation::{LocalCatalogBuilder, LocalContext, LocalError};
use crate::tool::output::ProducedOutput;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::super::{ImageSummary, workspace::relative_path};
use crate::tool::path::source_file;
use crate::{
    fs::FileKind,
    media::{Image, MAX_IMAGE_BYTES, MediaError},
    tool::output::{FinishedOutput, TextCaptureField},
    tool::{
        PathArgument, PathKind, RegistryError,
        policy::{Capability, PathAccess},
    },
};

pub(super) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    builder.register_product::<ReadArgs, ReadOutput, _, _>(
        crate::tool::builtins::names::READ,
        "Capture a complete UTF-8 file or directory listing, or attach a supported image. Use jobs to page or search the saved snapshot.",
        ToolOptions::new(vec![Capability::Read])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .argument_paths(|args: &mut ReadArgs| {
                vec![PathArgument::new(&mut args.path, PathAccess::Read, PathKind::Existing)]
            }),
        read,
    )?;
    Ok(())
}

async fn read(context: LocalContext, args: ReadArgs) -> Result<ProducedOutput, LocalError> {
    let path = std::path::PathBuf::from(&args.path);
    if fs::metadata(&path)
        .await
        .map_err(source(Operation::Inspect, &path))?
        .is_dir()
    {
        let mut directory = fs::read_dir(&path)
            .await
            .map_err(source(Operation::ReadDirectory, &path))?;
        let mut entries = DirectoryGroups::default();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(source(Operation::ReadDirectory, &path))?
        {
            let metadata = fs::symlink_metadata(entry.path()).await.map_err(|error| {
                LocalError::io(error)
                    .operation(Operation::Inspect, Subject::DirectoryEntry(entry.path()))
            })?;
            entries.push(entry.file_name().to_string_lossy().into_owned(), &metadata);
        }
        entries.sort();
        return Ok(ProducedOutput::new(serde_json::to_value(
            ReadOutput::Directory { entries },
        )?));
    }

    if let Some(capture) = read_text(&path, &context).await? {
        return Ok(ProducedOutput::new(serde_json::to_value(ReadOutput::File {
            content: String::new(),
        })?)
        .with_captures(vec![capture]));
    }

    let bytes = crate::fs::read_regular(&path, MAX_IMAGE_BYTES)
        .await
        .map_err(source_file(&path))?;
    let image = Image::decode(bytes).await.map_err(|error| {
        let failed = match error {
            MediaError::Task(error) => return error.into(),
            MediaError::UnsupportedImage => {
                LocalError::failed("file is neither UTF-8 text nor a supported image")
            }
            error => LocalError::failed(error),
        };
        failed.operation(Operation::Deserialize, Subject::path(&path))
    })?;
    let name = relative_path(&context.execution_location().workspace, &path);
    let reference = context
        .store_image(Some(name), &image)
        .await
        .map_err(|error| error.operation(Operation::StoreImage, Subject::path(&path)))?;
    Ok(ProducedOutput::new(serde_json::to_value(ReadOutput::Image {
        image: ImageSummary::from(&reference),
    })?)
    .with_images(vec![reference]))
}

fn source(
    operation: Operation,
    path: &std::path::Path,
) -> impl FnOnce(std::io::Error) -> LocalError + use<> {
    let subject = Subject::path(path);
    move |error| LocalError::io(error).operation(operation, subject)
}

/// The captured text, or `None` when the file is not text.
async fn read_text(
    path: &std::path::Path,
    context: &LocalContext,
) -> Result<Option<FinishedOutput>, LocalError> {
    let mut capture = context
        .text_capture(TextCaptureField::Content)
        .await
        .map_err(|error| error.operation(Operation::CreateCapture, Subject::path(path)))?
        .open();
    let subject = Subject::path(path);
    let path = path.to_owned();
    let cancellation = context.cancellation_token().child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // Reading, capture writes and finishing run in one blocking owner; an abandoned
    // capture discards itself.
    tokio::task::spawn_blocking(move || -> Result<_, LocalError> {
        let mut input =
            crate::fs::open_regular_blocking(&path, u64::MAX).map_err(source_file(&path))?;
        match copy_text(
            &path,
            &mut input,
            |text| capture.write_text(text),
            &cancellation,
        )? {
            TextRead::Complete => Ok(Some(capture.finish().map_err(|error| {
                LocalError::io(error).operation(Operation::FinishCapture, Subject::path(&path))
            })?)),
            TextRead::NotText => Ok(None),
        }
    })
    .await
    .map_err(|error| {
        LocalError::failed(error)
            .operation(Operation::Wait, subject)
            .effects(Effects::OutputIncomplete)
    })?
}

#[derive(Debug, PartialEq)]
enum TextRead {
    Complete,
    NotText,
}

/// Stream UTF-8 text without buffering the file. Like `media::classify`, a NUL
/// byte means the file is not text.
fn copy_text(
    path: &std::path::Path,
    input: &mut impl std::io::Read,
    mut output: impl FnMut(&str) -> std::io::Result<()>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<TextRead, LocalError> {
    let mut buffer = [0; 64 * 1024];
    let mut pending = Vec::new();
    let cancelled = || {
        LocalError::cancelled()
            .operation(Operation::Read, Subject::path(path))
            .effects(Effects::OutputIncomplete)
    };
    let capture =
        |error| LocalError::io(error).operation(Operation::WriteCapture, Subject::path(path));
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let size = input
            .read(&mut buffer)
            .map_err(source(Operation::Read, path))?;
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        if buffer[..size].contains(&0) {
            return Ok(TextRead::NotText);
        }
        pending.extend_from_slice(&buffer[..size]);
        match std::str::from_utf8(&pending) {
            Ok(text) => {
                output(text).map_err(capture)?;
                pending.clear();
            }
            Err(error) if error.error_len().is_none() && size != 0 => {
                let valid = error.valid_up_to();
                output(std::str::from_utf8(&pending[..valid]).expect("validated UTF-8 prefix"))
                    .map_err(capture)?;
                pending.drain(..valid);
            }
            Err(_) => return Ok(TextRead::NotText),
        }
        if size == 0 {
            break;
        }
    }
    Ok(TextRead::Complete)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    /// File or directory path.
    path: String,
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
    fn push(&mut self, name: String, metadata: &std::fs::Metadata) {
        match FileKind::from(metadata.file_type()) {
            FileKind::File => self.files.push(DirectoryFile {
                name,
                bytes: metadata.len(),
            }),
            FileKind::Directory => self.directories.push(name),
            FileKind::Symlink => self.symlinks.push(name),
            FileKind::Other => self.other.push(name),
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
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::job::CancellationToken;
    use crate::tests::tool_executor;
    use crate::tool::diagnostic::{Cause, IoKind};

    /// Invalid UTF-8 and NUL bytes are not text rather than IO errors, real
    /// invalid data stays IO, and cancellation before or during a read is typed.
    #[test]
    fn text_copy_classifies_text_io_and_cancellation() {
        struct Reader(Option<CancellationToken>);
        impl std::io::Read for Reader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let Some(cancellation) = &self.0 else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "device data error",
                    ));
                };
                cancellation.cancel();
                buf[0] = b'x';
                Ok(1)
            }
        }
        let cancellation = CancellationToken::new();
        let copy = |mut reader: &mut dyn std::io::Read| {
            copy_text(
                std::path::Path::new("input.txt"),
                &mut reader,
                |_| Ok(()),
                &cancellation,
            )
        };
        for input in [&b"valid\xff"[..], b"valid\0"] {
            assert_eq!(copy(&mut &input[..]).unwrap(), TextRead::NotText);
        }
        assert!(matches!(
            copy(&mut Reader(None)).unwrap_err().diagnostic().cause,
            Cause::Io {
                kind: IoKind::InvalidData,
                ..
            }
        ));
        // The first reader cancels during copying; the second starts cancelled.
        for reader in [
            &mut Reader(Some(cancellation.clone())) as &mut dyn std::io::Read,
            &mut &b"data"[..],
        ] {
            assert_eq!(
                copy(reader).unwrap_err().diagnostic().cause,
                Cause::Cancelled
            );
        }
    }

    #[tokio::test]
    async fn capture_failures_name_their_stage() {
        use crate::execution::ExecutionLocation;
        use crate::tool::invocation::{
            LocalCatalog,
            tests::{Authorizations, CapturedOutput},
        };
        use crate::tool::output::OutputContext;
        use crate::tool::output::tests::{CaptureStage, FailingCapture};

        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source.txt"), "captured text")
            .await
            .unwrap();
        let catalog = LocalCatalog::builtins().unwrap();
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
            let context = LocalContext::new(
                ExecutionLocation::root(root.path().to_owned()),
                [Capability::Read].into_iter().collect(),
                Default::default(),
                CancellationToken::new(),
                output.clone(),
                Arc::new(Authorizations::default()),
            );
            let error = catalog
                .run("read", json!({"path":"source.txt"}), context, root.path())
                .await
                .unwrap_err();
            assert_eq!(error.diagnostic().context.operation, operation);
            assert!(matches!(
                error.diagnostic().cause,
                Cause::Io {
                    kind: IoKind::PermissionDenied,
                    ..
                }
            ));
            output.settle().await.unwrap();
        }
    }

    #[tokio::test]
    async fn read_captures_complete_snapshot() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) = tool_executor(runtime.jobs.clone(), runtime.root.path());
        let path = runtime.root.path().join("lines.txt");
        fs::write(&path, "one\ntwo\nthree\n").await.unwrap();
        let result = executor
            .run_host(&runtime.agent, "read", json!({"path":"lines.txt"}))
            .await
            .unwrap();
        assert_eq!(result.output.value["content"], "one\ntwo\nthree\n");
        fs::write(&path, "changed").await.unwrap();
        let mut args = crate::job::output::OutputArgs::new(result.job);
        (args.field, args.start, args.limit) = (
            Some("/result/content".parse().unwrap()),
            Some(2),
            Some(crate::job::output::PageLimit::new::<1>()),
        );
        let page = runtime
            .jobs
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(page["presentation"]["preview"]["lines"][0], "two");
    }

    #[tokio::test]
    async fn directories_group_entries_by_their_own_metadata() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) = tool_executor(runtime.jobs.clone(), runtime.root.path());
        let listing = runtime.root.path().join("listing");
        std::fs::create_dir_all(listing.join("directory")).unwrap();
        std::fs::write(listing.join("file"), b"abc").unwrap();
        std::os::unix::fs::symlink(listing.join("file"), listing.join("link")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(listing.join("socket")).unwrap();
        let read = async |path| {
            let result = executor.run_host(&runtime.agent, "read", json!({"path": path}));
            result.await.unwrap().output.value
        };
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
