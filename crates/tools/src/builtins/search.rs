use crate::diagnostic::PartialContext;
use crate::output::ProducedOutput;
use crate::spec::TargetTool;
use crate::{
    error::{AdmissionError, CallError},
    spec::{Call, ended},
};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};
use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::output::{FinishedOutput, OutputWriter, PendingOutput};
use crate::path::{PathAccess, PathArgument};
use skyhook_worker::{
    op,
    op::{Case, PathKind, Skipped},
};

/// How many batches of a stream's events render at once.
const RENDERERS: usize = 8;

/// Stream `running`'s events into a capture of the `field` it fills in `tool`'s result,
/// shaped as `shape`, each as `render` renders it and `push` then writes it, until the
/// operation ends or `call` is cancelled while doing `operation` on `root`; its end and
/// the capture. The events that waited together render as a batch, several batches at
/// once, and are written in the order they arrived; writing a capture blocks, so both
/// run on the blocking pool.
async fn stream<O: op::Operation, U: Into<CallError> + Send + 'static, R: Send + 'static>(
    mut running: op::Running<O, U>,
    call: &Call,
    (tool, field, shape): (&str, &str, Shape),
    (operation, root): (Operation, op::TargetPath),
    render: impl Fn(O::Event) -> R + Send + Sync + 'static,
    mut push: impl FnMut(&mut CapturedOutput, R) -> std::io::Result<()> + Send + 'static,
) -> Result<(O::End, FinishedOutput), CallError>
where
    O::Error: Into<CallError>,
{
    let capture = (call.pending_stream_capture(
        crate::output::FieldPointer::result().property(field),
        crate::output::CaptureKind::Json,
    ))
    .await
    .map_err(|error| {
        let subject = Subject::Label(format!("{tool} {field}"));
        error.operation(Operation::CreateCapture, subject)
    })?;
    // Dropping this call stops the writer, which then drops the operation, as
    // cancelling the call does.
    let stop = call.cancellation_token().child_token();
    let _stop = stop.clone().drop_guard();
    let runtime = tokio::runtime::Handle::current();
    let render = std::sync::Arc::new(render);
    let written = async move {
        let mut output = CapturedOutput::new(capture, shape)
            .map_err(|error| capture_error(error, Operation::WriteCapture))?;
        // The batches rendering on other threads, oldest first: each is written once it
        // and those before it are rendered. An event that waited alone while none is
        // rendering is rendered here, since handing it over takes longer than it does.
        let mut rendering = std::collections::VecDeque::new();
        let mut open = true;
        while open || !rendering.is_empty() {
            let rendered: Vec<R> = tokio::select! {
                () = stop.cancelled() => {
                    return Err(CallError::cancelled()
                        .operation(operation, Subject::path(&root))
                        .effects(Effects::OutputIncomplete));
                }
                Some(rendered) = async { Some(rendering.front_mut()?.await) } => {
                    rendering.pop_front();
                    skyhook_base::joined(rendered).await
                }
                events = running.events(), if open && rendering.len() < RENDERERS => {
                    open = !events.is_empty();
                    let alone = events.len() <= 1 && rendering.is_empty();
                    let render = render.clone();
                    let batch = move || events.into_iter().map(|event| render(event)).collect();
                    if !alone {
                        rendering.push_back(skyhook_base::spawn_blocking(batch));
                        continue;
                    }
                    batch()
                }
            };
            for entry in rendered {
                push(&mut output, entry)
                    .map_err(|error| capture_error(error, Operation::WriteCapture))?;
            }
        }
        let end = ended(running).await?.map_err(Into::into)?;
        let output =
            (output.finish()).map_err(|error| capture_error(error, Operation::FinishCapture))?;
        Ok((end, output))
    };
    skyhook_base::blocking(move || runtime.block_on(written)).await
}

/// Search file contents.
pub struct Search;

impl TargetTool for Search {
    const NAME: &'static str = "search";
    const DESCRIPTION: &'static str = "Search files with ripgrep regex, glob, and ignore semantics. Matches map file paths to arrays of \"line: text\" strings; a long line is cut to a window around its match. Files that could not be searched are listed in `skipped`, and then `matches` is incomplete";
    type Input = SearchArgs;
    type Admitted = SearchArgs;
    type Op = op::Search;
    type Rendering = op::TargetPath;

    fn result_schema() -> Option<serde_json::Value> {
        Some(crate::schema::result_schema::<SearchOutput>())
    }

    fn admit(input: SearchArgs) -> Result<SearchArgs, AdmissionError> {
        Ok(input)
    }

    fn paths(args: &mut SearchArgs) -> Vec<PathArgument<'_>> {
        vec![PathArgument::new(
            &mut args.path,
            PathAccess::Read,
            PathKind::Existing,
        )]
    }

    fn op(args: &mut SearchArgs, call: &Call) -> Result<op::Search, CallError> {
        Ok(op::Search {
            root: args.path.clone(),
            workspace: call.execution_location().workspace.clone(),
            pattern: std::mem::take(&mut args.pattern),
            fixed: args.fixed,
            word: args.word,
            case: args.case,
            globs: args.glob.as_slice().to_vec(),
            hidden: args.hidden,
            no_ignore: args.no_ignore,
        })
    }

    fn rendering(args: SearchArgs) -> op::TargetPath {
        args.path
    }

    async fn render<U: Into<CallError> + Send + 'static>(
        root: op::TargetPath,
        running: op::Running<op::Search, U>,
        call: &Call,
    ) -> Result<ProducedOutput, CallError> {
        // A part's lines, joined as they stand in their file's group.
        let lines = |event| {
            let (path, lines, last) = match event {
                op::SearchEvent::Part { path, lines } => (path, lines, false),
                op::SearchEvent::File { path, lines } => (path, lines, true),
            };
            let mut rendered = Vec::new();
            for line in &lines {
                if !rendered.is_empty() {
                    rendered.extend_from_slice(b",\n");
                }
                let line = format!("{}: {}", line.number, shown(&line.excerpt));
                render(&mut rendered, &line);
            }
            (path, rendered, last)
        };
        // Each file's group as rendered so far, until its last part: files searched in
        // parallel interleave their parts.
        let mut pending = std::collections::HashMap::<String, Vec<u8>>::new();
        let group = move |matches: &mut CapturedOutput,
                          (path, lines, last): (String, Vec<u8>, bool)| {
            let mut group = pending.remove(&path).unwrap_or_else(|| {
                let mut group = Vec::new();
                render(&mut group, &path);
                group.extend_from_slice(b": [");
                group
            });
            if !lines.is_empty() {
                // A group holding lines ends with a quote, so only its first follow `[`.
                let separator: &[u8] = if group.ends_with(b"[") { b"\n" } else { b",\n" };
                group.extend_from_slice(separator);
                group.extend_from_slice(&lines);
            }
            if !last {
                pending.insert(path, group);
                return Ok(());
            }
            group.extend_from_slice(b"\n]");
            matches.push_rendered(&group)
        };
        let capture = ("search", "matches", Shape::Object);
        let reading = (Operation::Read, root);
        let (skipped, matches) = stream(running, call, capture, reading, lines, group).await?;
        let value = SearchOutput {
            matches: BTreeMap::new(),
            skipped,
        };
        Ok(ProducedOutput::serialized(value)?.with_captures(vec![matches]))
    }
}

/// Find files by glob.
pub struct Glob;

impl TargetTool for Glob {
    const NAME: &'static str = "glob";
    const DESCRIPTION: &'static str = "Find files with ripgrep glob and ignore semantics";
    type Input = GlobArgs;
    type Admitted = GlobArgs;
    type Op = op::Glob;
    type Rendering = op::TargetPath;

    fn result_schema() -> Option<serde_json::Value> {
        Some(crate::schema::result_schema::<GlobOutput>())
    }

    fn admit(input: GlobArgs) -> Result<GlobArgs, AdmissionError> {
        Ok(input)
    }

    fn paths(args: &mut GlobArgs) -> Vec<PathArgument<'_>> {
        vec![PathArgument::new(
            &mut args.path,
            PathAccess::Read,
            PathKind::Existing,
        )]
    }

    fn op(args: &mut GlobArgs, call: &Call) -> Result<op::Glob, CallError> {
        Ok(op::Glob {
            root: args.path.clone(),
            workspace: call.execution_location().workspace.clone(),
            pattern: std::mem::take(&mut args.pattern),
            hidden: args.hidden,
            no_ignore: args.no_ignore,
        })
    }

    fn rendering(args: GlobArgs) -> op::TargetPath {
        args.path
    }

    async fn render<U: Into<CallError> + Send + 'static>(
        root: op::TargetPath,
        running: op::Running<op::Glob, U>,
        call: &Call,
    ) -> Result<ProducedOutput, CallError> {
        let capture = ("glob", "paths", Shape::Array);
        let listing = (Operation::ReadDirectory, root);
        let push = |paths: &mut CapturedOutput, found: Vec<String>| {
            found.iter().try_for_each(|path| paths.push(path))
        };
        let ((), paths) = stream(running, call, capture, listing, |found| found, push).await?;
        Ok(ProducedOutput::new(serde_json::json!({})).with_captures(vec![paths]))
    }
}

fn capture_error(error: std::io::Error, operation: Operation) -> CallError {
    let capture = PartialContext::new(operation, Subject::Label("result capture".into()));
    CallError::io(error).or(capture.effects(Effects::OutputIncomplete))
}

/// Streams the complete result into the owning job's capture, one entry at a time. The
/// handler returns completed ownership evidence; the finalizer publishes the captured
/// field.
struct CapturedOutput {
    file: OutputWriter,
    shape: Shape,
    empty: bool,
}

#[derive(Clone, Copy)]
enum Shape {
    Array,
    Object,
}

impl CapturedOutput {
    fn new(capture: PendingOutput, shape: Shape) -> std::io::Result<Self> {
        use std::io::Write as _;
        let mut file = capture.open();
        file.write_all(match shape {
            Shape::Array => b"[\n",
            Shape::Object => b"{\n",
        })?;
        Ok(Self {
            file,
            shape,
            empty: true,
        })
    }

    fn separate(&mut self) -> std::io::Result<()> {
        use std::io::Write as _;
        if !std::mem::replace(&mut self.empty, false) {
            self.file.write_all(b",\n")?;
        }
        Ok(())
    }

    /// Append `value` to the array.
    fn push(&mut self, value: impl Serialize) -> std::io::Result<()> {
        self.separate()?;
        Ok(serde_json::to_writer(&mut self.file, &value)?)
    }

    /// Append one entry already rendered as JSON, such as an object's `"key": value`.
    fn push_rendered(&mut self, entry: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        self.separate()?;
        self.file.write_all(entry)
    }

    fn finish(mut self) -> std::io::Result<FinishedOutput> {
        use std::io::Write as _;
        self.file.write_all(match self.shape {
            Shape::Array => b"\n]",
            Shape::Object => b"\n}",
        })?;
        self.file.finish()
    }
}

/// Append `text` to `out` as a JSON string.
fn render(out: &mut Vec<u8>, text: &impl Serialize) {
    serde_json::to_writer(out, text).expect("text serializes into memory");
}

/// A matched line as shown: whole, or its window marked with how long the line was.
fn shown(excerpt: &op::Excerpt) -> String {
    match excerpt.length {
        None => excerpt.text.clone(),
        Some(length) => format!(
            "{}{}{} (line truncated, {length} bytes)",
            if excerpt.cut_before { "…" } else { "" },
            excerpt.text,
            if excerpt.cut_after { "…" } else { "" },
        ),
    }
}

/// `line` whole when it is short; otherwise the window around `found`, its match,
/// marked with how long the line was.
pub fn excerpt(line: &[u8], found: Option<std::ops::Range<usize>>) -> String {
    shown(&skyhook_worker::op::excerpt(line, found))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// Rust-regex pattern, using ripgrep syntax
    pattern: String,
    /// File or directory to search
    #[serde(default = "super::default_dot")]
    path: op::TargetPath,
    /// Ripgrep-style include/exclude globs. Prefix exclusions with `!`; later globs override earlier ones
    #[serde(default)]
    glob: super::OneOrMany<String>,
    /// Treat `pattern` as literal text instead of a regular expression (`rg -F`)
    #[serde(default)]
    fixed: bool,
    #[serde(default)]
    case: Case,
    /// Require whole-word matches (`rg -w`)
    #[serde(default)]
    word: bool,
    /// Include hidden files and directories (`rg --hidden`)
    #[serde(default)]
    hidden: bool,
    /// Ignore .gitignore and related ignore files (`rg --no-ignore`)
    #[serde(default)]
    no_ignore: bool,
}

#[derive(Serialize, JsonSchema)]
struct SearchOutput {
    matches: BTreeMap<String, Vec<String>>,
    /// Files the search could not cover; when present, `matches` is incomplete
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<Skipped>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobArgs {
    /// Ripgrep-style file glob, such as `src/**/*.rs`
    pattern: String,
    /// Directory to enumerate
    #[serde(default = "super::default_dot")]
    path: op::TargetPath,
    /// Include hidden files and directories
    #[serde(default)]
    hidden: bool,
    /// Ignore .gitignore and related ignore files
    #[serde(default)]
    no_ignore: bool,
}

#[derive(Serialize, JsonSchema)]
struct GlobOutput {
    paths: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::diagnostic::Cause;
    use crate::output::capture::tests::{CaptureStage, FailingCapture};
    use serde_json::json;
    use skyhook_worker::op::IoKind;
    use skyhook_worker::target::Target as _;
    use std::sync::Arc;

    #[tokio::test]
    async fn invalid_patterns_identify_their_arguments() {
        let workspace = crate::spec::tests::Workspace::new();
        let invalid = "[z-a]";
        for (tool, arguments, argument) in [
            ("search", json!({"pattern":invalid}), "/pattern"),
            (
                "search",
                json!({"pattern":"needle", "glob":["*.rs", invalid]}),
                "/glob/1",
            ),
            ("glob", json!({"pattern":invalid}), "/pattern"),
        ] {
            let error = workspace.run(tool, arguments).await.unwrap_err();
            let diagnostic = error.diagnostic();
            assert_matches!(diagnostic.cause, Cause::InvalidArguments(_));
            assert_eq!(diagnostic.context.operation, Operation::Validate);
            assert_eq!(
                diagnostic.context.subject,
                Subject::Argument(argument.into())
            );
        }
    }

    #[test]
    fn long_lines_show_their_window_and_length() {
        let line = format!("{}end", "x".repeat(2000));
        let shown = excerpt(line.as_bytes(), Some(2000..2004));
        assert!(
            shown.starts_with('…') && shown.ends_with("end (line truncated, 2003 bytes)"),
            "{shown}"
        );
        assert_eq!(excerpt(b"short", None), "short");
    }

    #[tokio::test]
    async fn capture_failures_preserve_their_phase() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        std::fs::write(&input, "needle\n".repeat(20_000)).unwrap();
        let local = skyhook_worker::local::Local::new(root.path().to_owned());
        for (stage, failure, skip) in [
            (CaptureStage::Write, Operation::WriteCapture, 1),
            (CaptureStage::Finish, Operation::FinishCapture, 0),
        ] {
            let sink = Arc::new(crate::spec::tests::CapturedOutput::default());
            let output = crate::output::OutputContext::new(Arc::new(FailingCapture::new(
                sink,
                stage,
                skip,
                || std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )));
            let search = op::Search {
                root: op::TargetPath::new(input.to_str().unwrap()),
                workspace: op::TargetPath::new(root.path().to_str().unwrap()),
                pattern: "needle".into(),
                fixed: false,
                word: false,
                case: Case::Smart,
                globs: Vec::new(),
                hidden: false,
                no_ignore: false,
            };
            let running = local.run(search).await.unwrap();
            let call = crate::spec::tests::call(root.path(), output.clone(), Default::default());
            let rendered = Search::render(crate::tests::text(&input), running, &call).await;
            let diagnostic = rendered.unwrap_err().diagnostic();
            assert_eq!(diagnostic.context.operation, failure);
            assert_eq!(
                diagnostic.context.subject,
                Subject::Label("result capture".into())
            );
            assert_eq!(diagnostic.context.effects, Effects::OutputIncomplete);
            assert_matches!(
                diagnostic.cause,
                Cause::Io(skyhook_worker::op::OsError {
                    kind: IoKind::PermissionDenied,
                    ..
                })
            );
            crate::tests::bounded(output.settle()).await.unwrap();
        }
    }

    /// Files searched in parallel interleave their parts; each match stays its own file's.
    #[tokio::test]
    async fn interleaved_parts_stay_with_their_files() {
        let lines = |path: &str, numbers: std::ops::RangeInclusive<u64>| {
            let line = |number| {
                let text = path.to_owned();
                let (cut_before, cut_after, length) = (false, false, None);
                let excerpt = op::Excerpt {
                    text,
                    cut_before,
                    cut_after,
                    length,
                };
                op::MatchLine { number, excerpt }
            };
            (path.to_owned(), numbers.map(line).collect::<Vec<_>>())
        };
        let part = |(path, lines)| op::SearchEvent::Part { path, lines };
        let file = |(path, lines)| op::SearchEvent::File { path, lines };
        let events = [
            part(lines("a", 1..=2)),
            part(lines("b", 1..=2)),
            file(lines("a", 3..=3)),
            file(lines("b", 3..=3)),
        ];
        let (sender, receiver) = tokio::sync::mpsc::channel(events.len());
        for event in events {
            sender.try_send(event).unwrap();
        }
        drop(sender);
        let running: op::Running<op::Search, CallError> =
            op::Running::new(receiver, async { Ok(Ok(Vec::new())) }, ());
        let sink = Arc::new(crate::spec::tests::CapturedOutput::default());
        let output = crate::output::OutputContext::new(sink.clone());
        let root = tempfile::tempdir().unwrap();
        let call = crate::spec::tests::call(root.path(), output.clone(), Default::default());
        let root = crate::tests::text(root.path());
        Search::render(root, running, &call).await.unwrap();
        crate::tests::bounded(output.settle()).await.unwrap();
        let matches: BTreeMap<String, Vec<String>> = serde_json::from_slice(&sink.bytes()).unwrap();
        for name in ["a", "b"] {
            let expected: Vec<_> = (1..=3).map(|line| format!("{line}: {name}")).collect();
            assert_eq!(matches[name], expected, "{name}");
        }
    }

    #[tokio::test]
    async fn grouped_search_preserves_whitespace_and_drops_binary_files() {
        let workspace = crate::spec::tests::Workspace::new();
        let directory = workspace.path().join("files");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("a.txt"), "  needle  \nneedle again\n").unwrap();
        std::fs::write(directory.join("b.bin"), b"needle\n\0hidden").unwrap();
        std::fs::write(directory.join("z.txt"), "needle last\n").unwrap();
        let search = async |args| workspace.run("search", args).await.unwrap();
        // Paths are spelled natively.
        let native = |name| {
            std::path::Path::new("files")
                .join(name)
                .into_os_string()
                .into_string()
                .unwrap()
        };
        let grouped = json!({"matches":{
            (native("a.txt")):["1:   needle  ", "2: needle again"], (native("z.txt")):["1: needle last"]
        }});
        assert_eq!(
            search(json!({"path":"files","pattern":"needle"})).await,
            grouped
        );
        let empty = search(json!({"path":"files","pattern":"absent"})).await;
        assert_eq!(empty, json!({"matches":{}}));
    }

    /// A file or directory the search cannot read is listed and skipped; the other
    /// files' matches stay.
    #[tokio::test]
    async fn unreadable_entries_are_skipped() {
        use std::os::unix::fs::PermissionsExt as _;
        let workspace = crate::spec::tests::Workspace::new();
        let directory = workspace.path().join("files");
        for file in ["closed/inner.txt", "locked.txt", "open.txt"] {
            let path = directory.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "needle\n").unwrap();
        }
        let mode = |path: &str, mode| {
            std::fs::set_permissions(directory.join(path), std::fs::Permissions::from_mode(mode))
                .unwrap();
        };
        mode("closed", 0o000);
        mode("locked.txt", 0o000);
        let searched = workspace.run("search", json!({"path":"files","pattern":"needle"}));
        let searched = searched.await;
        mode("closed", 0o700);
        let value = searched.unwrap();
        assert_eq!(value["matches"]["files/open.txt"], json!(["1: needle"]));
        assert_eq!(
            value["skipped"],
            json!([
                {"path":"files/closed","reason":"unlisted"},
                {"path":"files/locked.txt","reason":"unopened"},
            ])
        );
    }
}
