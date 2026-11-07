use crate::tool::ToolOptions;
use crate::tool::diagnostic::{Effects, Operation, PartialContext, Subject};
use crate::tool::invocation::{LocalCatalogBuilder, LocalContext, LocalError};
use crate::tool::output::ProducedOutput;
use crate::tool::registry::Invocation;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Mutex, OnceLock, PoisonError},
};

use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::workspace::relative_path;

/// The most of a matched line kept on each side of its match; a longer line is cut
/// to this window and says how long it was.
const MATCH_CONTEXT_BYTES: usize = 256;
use crate::tool::output::{FinishedOutput, OutputWriter, PendingOutput};
use crate::tool::{
    PathArgument, PathKind, RegistryError,
    policy::{Capability, PathAccess},
};

pub(super) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    builder.register_checked(
        "search",
        "Search files with ripgrep regex, glob, and ignore semantics. Matches map file paths to arrays of \"line: text\" strings; a long line is cut to a window around its match. Files that could not be searched are listed in `skipped`, and then `matches` is incomplete",
        ToolOptions::new(vec![Capability::Read])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .argument_paths(|args: &mut SearchArgs| {
                vec![PathArgument::new(&mut args.path, PathAccess::Read, PathKind::Existing)]
            })
            .result::<SearchOutput>(),
        Ok,
        |args: SearchArgs| Invocation::new(|context| async move {
            run_walker(context, "search", "matches", move |workspace, capture, cancel| {
                let (matches, skipped) =
                    search_blocking(workspace, Path::new(&args.path), &args, capture, cancel)?;
                let value = SearchOutput { matches: BTreeMap::new(), skipped };
                Ok((matches, serde_json::to_value(value)?))
            })
            .await
        }),
    )?;
    builder.register_checked(
        "glob",
        "Find files with ripgrep glob and ignore semantics",
        ToolOptions::new(vec![Capability::Read])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .argument_paths(|args: &mut GlobArgs| {
                vec![PathArgument::new(
                    &mut args.path,
                    PathAccess::Read,
                    PathKind::Existing,
                )]
            })
            .result::<GlobOutput>(),
        Ok,
        |args: GlobArgs| {
            Invocation::new(|context| async move {
                run_walker(
                    context,
                    "glob",
                    "paths",
                    move |workspace, capture, cancel| {
                        let paths = glob_blocking(
                            workspace,
                            Path::new(&args.path),
                            &args,
                            capture,
                            cancel,
                        )?;
                        Ok((paths, serde_json::json!({})))
                    },
                )
                .await
            })
        },
    )?;
    Ok(())
}

/// Walk on a blocking thread, streaming the result's `field` into its capture; the
/// walk also returns the rest of the result.
async fn run_walker(
    context: LocalContext,
    tool: &str,
    field: &str,
    walk: impl FnOnce(
        &Path,
        PendingOutput,
        &tokio_util::sync::CancellationToken,
    ) -> Result<(FinishedOutput, serde_json::Value), LocalError>
    + Send
    + 'static,
) -> Result<ProducedOutput, LocalError> {
    let capture = context
        .pending_stream_capture(
            crate::tool::output::FieldPointer::result().property(field),
            crate::tool::output::CaptureKind::Json,
        )
        .await
        .map_err(|error| {
            let subject = Subject::Label(format!("{tool} {field}"));
            error.operation(Operation::CreateCapture, subject)
        })?;
    let workspace = context.execution_location().workspace.clone();
    let (capture, value) = tokio::task::spawn_blocking(move || {
        walk(&workspace, capture, &context.cancellation_token())
    })
    .await
    .map_err(|error| {
        LocalError::failed(error)
            .operation(Operation::Wait, Subject::Label(format!("{tool} worker")))
            .effects(Effects::OutputIncomplete)
    })??;
    Ok(ProducedOutput::new(value).with_captures(vec![capture]))
}

fn walk_builder<'a>(
    root: &Path,
    hidden: bool,
    no_ignore: bool,
    patterns: impl IntoIterator<Item = (&'a str, Subject)>,
) -> Result<WalkBuilder, LocalError> {
    let patterns: Vec<_> = patterns.into_iter().collect();
    let named = Named::new(patterns.iter().map(|(pattern, _)| *pattern));
    let patterns = overrides(root, patterns)?;
    let root = root.to_owned();
    let mut walk = WalkBuilder::new(&root);
    // Hidden entries are filtered here rather than by the walker, so a pattern can name one.
    walk.hidden(false)
        .ignore(!no_ignore)
        .git_ignore(!no_ignore)
        .git_exclude(!no_ignore)
        .git_global(false)
        .parents(!no_ignore)
        .follow_links(false)
        // Apply globs after normal ignore eligibility, including directory
        // exclusions so excluded subtrees are never traversed.
        .filter_entry(move |entry| {
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            entry.depth() == 0
                || ((hidden
                    || !entry.file_name().as_encoded_bytes().starts_with(b".")
                    || named.names(&root, entry.path()))
                    && !patterns.matched(entry.path(), is_dir).is_ignore())
        });
    Ok(walk)
}

/// The leading literal segments of each positive pattern: a hidden entry they spell out,
/// such as `.github` in `.github/*.yml`, is walked even when hidden entries are skipped.
struct Named(Vec<(bool, Vec<String>)>);

impl Named {
    fn new<'a>(patterns: impl Iterator<Item = &'a str>) -> Self {
        let wild = |segment: &str| segment.contains(['*', '?', '[', '{', '\\']);
        Self(
            patterns
                .filter(|pattern| !pattern.starts_with('!'))
                .map(|pattern| {
                    let pattern = pattern.trim_end_matches('/');
                    let (anchored, rest) = match pattern.strip_prefix("**/") {
                        Some(rest) => (false, rest),
                        // As in gitignore, a slash before the end anchors a pattern.
                        None => (pattern.contains('/'), pattern.trim_start_matches('/')),
                    };
                    let literal = rest.split('/').take_while(|segment| !wild(segment));
                    (anchored, literal.map(str::to_owned).collect())
                })
                .collect(),
        )
    }

    /// Whether `path` beneath `root` is one of the literal prefixes or an ancestor of one.
    fn names(&self, root: &Path, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let segments: Vec<_> = relative.iter().map(|part| part.to_string_lossy()).collect();
        let spells = |literal: &[String], start: usize| {
            let tail = &segments[start..];
            !tail.is_empty()
                && tail.len() <= literal.len()
                && tail.iter().zip(literal).all(|(a, b)| a == b)
        };
        self.0.iter().any(|(anchored, literal)| match anchored {
            true => spells(literal, 0),
            false => (0..segments.len()).any(|start| spells(literal, start)),
        })
    }
}

fn overrides<'a>(
    root: &Path,
    patterns: impl IntoIterator<Item = (&'a str, Subject)>,
) -> Result<ignore::overrides::Override, LocalError> {
    let mut builder = OverrideBuilder::new(root);
    for (pattern, subject) in patterns {
        // Upstream diagnostics embed the pattern; identify its argument instead.
        builder.add(pattern).map_err(|_| {
            LocalError::invalid_arguments("glob pattern could not be compiled")
                .operation(Operation::Validate, subject)
        })?;
    }
    builder.build().map_err(|_| {
        LocalError::invalid_arguments("glob patterns could not be compiled")
            .operation(Operation::Validate, Subject::Label("glob patterns".into()))
    })
}

fn search_blocking(
    workspace: &Path,
    root: &Path,
    args: &SearchArgs,
    capture: PendingOutput,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<(FinishedOutput, Vec<Skipped>), LocalError> {
    let mut matcher_builder = crate::job::output::line_matcher();
    matcher_builder.fixed_strings(args.fixed).word(args.word);
    match args.case {
        Case::Smart => {
            matcher_builder.case_smart(true);
        }
        Case::Sensitive => {
            matcher_builder.case_insensitive(false).case_smart(false);
        }
        Case::Insensitive => {
            matcher_builder.case_insensitive(true).case_smart(false);
        }
    }
    let matcher = matcher_builder.build(&args.pattern).map_err(|_| {
        LocalError::invalid_arguments("regular expression could not be compiled")
            .operation(Operation::Validate, Subject::argument(["pattern"]))
    })?;

    let metadata = std::fs::metadata(root).map_err(LocalError::annotated(PartialContext::new(
        Operation::Inspect,
        Subject::path(root),
    )))?;
    let walk = if metadata.is_file() {
        None
    } else if metadata.is_dir() {
        let patterns = args.glob.as_slice().iter().enumerate();
        Some(walk_builder(
            root,
            args.hidden,
            args.no_ignore,
            patterns.map(|(index, pattern)| {
                (
                    pattern.as_str(),
                    Subject::argument(["glob", &index.to_string()]),
                )
            }),
        )?)
    } else {
        return Err(LocalError::failed("search root is not a file or directory")
            .operation(Operation::Inspect, Subject::path(root)));
    };
    let search = Search {
        workspace,
        root,
        matcher,
        searcher: {
            let mut searcher = SearcherBuilder::new();
            searcher
                .line_number(true)
                .binary_detection(BinaryDetection::quit(b'\0'))
                .heap_limit(Some(crate::limits::WHOLE_VALUE_BYTES));
            searcher
        },
        matches: Mutex::new(
            CapturedOutput::new(capture, Shape::Object)
                .map_err(|error| capture_error(error, Operation::WriteCapture))?,
        ),
        skipped: Mutex::default(),
        stop: OnceLock::new(),
        cancellation,
    };
    match walk {
        None => {
            search.unless_cancelled()?;
            search.worker().file(root, true)?
        }
        // Each file is searched whole by one worker, which appends its matches as one
        // group, so groups land in the order their files finish.
        Some(walk) => walk.build_parallel().run(|| {
            let mut worker = search.worker();
            Box::new(move |entry| worker.visit(entry))
        }),
    }
    let Search {
        matches,
        skipped,
        stop,
        ..
    } = search;
    if let Some(stop) = stop.into_inner() {
        return Err(stop);
    }
    let matches = (matches.into_inner().unwrap_or_else(PoisonError::into_inner))
        .finish()
        .map_err(|error| capture_error(error, Operation::FinishCapture))?;
    let mut skipped = skipped.into_inner().unwrap_or_else(PoisonError::into_inner);
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((matches, skipped))
}

/// One search, shared by the threads walking it.
struct Search<'a> {
    workspace: &'a Path,
    root: &'a Path,
    matcher: grep_regex::RegexMatcher,
    searcher: SearcherBuilder,
    matches: Mutex<CapturedOutput>,
    skipped: Mutex<Vec<Skipped>>,
    /// Why the search ended early, the first time it did.
    stop: OnceLock<LocalError>,
    cancellation: &'a tokio_util::sync::CancellationToken,
}

impl Search<'_> {
    fn worker(&self) -> Worker<'_> {
        Worker {
            search: self,
            matcher: self.matcher.clone(),
            searcher: self.searcher.build(),
            group: Vec::new(),
        }
    }

    fn unless_cancelled(&self) -> Result<(), LocalError> {
        if self.cancellation.is_cancelled() {
            return Err(LocalError::cancelled()
                .operation(Operation::Read, Subject::path(self.root))
                .effects(Effects::OutputIncomplete));
        }
        Ok(())
    }

    fn skip(&self, path: &Path, reason: SkipReason) {
        let skipped = Skipped {
            path: relative_path(self.workspace, path),
            reason,
        };
        (self.skipped.lock().unwrap_or_else(PoisonError::into_inner)).push(skipped);
    }
}

/// One thread's share of a search, with its own matcher and searcher, as ripgrep keeps.
struct Worker<'a> {
    search: &'a Search<'a>,
    matcher: grep_regex::RegexMatcher,
    searcher: Searcher,
    /// The file being searched's matches, rendered as its group's JSON.
    group: Vec<u8>,
}

impl Worker<'_> {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState {
        let searched = self.search.unless_cancelled().and_then(|()| match entry {
            Ok(entry) if entry.file_type().is_some_and(|kind| kind.is_file()) => {
                self.file(entry.path(), false)
            }
            Ok(_) => Ok(()),
            Err(error) => {
                let path = error_path(&error).unwrap_or(self.search.root);
                self.search.skip(path, SkipReason::Unlisted);
                Ok(())
            }
        });
        match searched {
            Ok(()) => ignore::WalkState::Continue,
            Err(stop) => {
                let _ = self.search.stop.set(stop);
                ignore::WalkState::Quit
            }
        }
    }

    /// Search `path`, appending its matches once it was read whole; `single` when it is
    /// the search's root rather than a file the walk found.
    fn file(&mut self, path: &Path, single: bool) -> Result<(), LocalError> {
        let search = self.search;
        // A live database's file is never opened: a walk passes over it, and a search
        // of that one file is refused.
        let file = match crate::fs::open_regular_blocking(path, u64::MAX) {
            Ok(file) => file,
            Err(crate::fs::RegularFileError::Live(live)) if single => {
                return Err(LocalError::from(crate::fs::RegularFileError::Live(live))
                    .operation(Operation::Read, Subject::path(path)));
            }
            Err(crate::fs::RegularFileError::Live(_)) => return Ok(()),
            Err(_) => {
                search.skip(path, SkipReason::Unopened);
                return Ok(());
            }
        };
        self.group.clear();
        let mut sink = SearchSink {
            path,
            workspace: search.workspace,
            group: &mut self.group,
            matcher: &self.matcher,
            binary: false,
            cancellation: search.cancellation,
        };
        let searched = self.searcher.search_file(&self.matcher, &file, &mut sink);
        // A file that stopped partway keeps none of its matches, like a binary one.
        match searched {
            Ok(()) if sink.binary || self.group.is_empty() => {}
            Ok(()) => {
                self.group.extend_from_slice(b"\n]");
                (search
                    .matches
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner))
                .push_rendered(&self.group)
                .map_err(|error| capture_error(error, Operation::WriteCapture))?;
            }
            Err(SearchStop::Read) => search.skip(path, SkipReason::Unread),
            Err(SearchStop::Cancelled) => {
                return Err(LocalError::cancelled()
                    .operation(Operation::Read, Subject::path(path))
                    .effects(Effects::OutputIncomplete));
            }
        }
        Ok(())
    }
}

fn error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        ignore::Error::Partial(errors) => errors.first().and_then(error_path),
        ignore::Error::Loop { child, .. } => Some(child),
        _ => None,
    }
}

fn glob_blocking(
    workspace: &Path,
    root: &Path,
    args: &GlobArgs,
    capture: PendingOutput,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<FinishedOutput, LocalError> {
    let metadata = std::fs::metadata(root).map_err(LocalError::annotated(PartialContext::new(
        Operation::Inspect,
        Subject::path(root),
    )))?;
    if !metadata.is_dir() {
        return Err(LocalError::failed("glob root is not a directory")
            .operation(Operation::Inspect, Subject::path(root)));
    }
    let mut walk = walk_builder(
        root,
        args.hidden,
        args.no_ignore,
        std::iter::once((args.pattern.as_str(), Subject::argument(["pattern"]))),
    )?;
    walk.sort_by_file_path(std::cmp::Ord::cmp);
    let mut paths = CapturedOutput::new(capture, Shape::Array)
        .map_err(|error| capture_error(error, Operation::WriteCapture))?;
    for entry in walk.build() {
        if cancellation.is_cancelled() {
            return Err(LocalError::cancelled()
                .operation(Operation::ReadDirectory, Subject::path(root))
                .effects(Effects::OutputIncomplete));
        }
        let entry = entry.map_err(|error| walk_error(error, root))?;
        if entry.depth() != 0 && entry.file_type().is_some_and(|kind| kind.is_file()) {
            let path = relative_path(workspace, entry.path());
            paths
                .push(&path)
                .map_err(|error| capture_error(error, Operation::WriteCapture))?;
        }
    }
    paths
        .finish()
        .map_err(|error| capture_error(error, Operation::FinishCapture))
}

fn walk_error(error: ignore::Error, root: &Path) -> LocalError {
    let error = match error {
        ignore::Error::WithPath { path, err } => return walk_error(*err, &path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            return walk_error(*err, root);
        }
        ignore::Error::Partial(mut errors) if errors.len() == 1 => {
            return walk_error(errors.remove(0), root);
        }
        ignore::Error::Io(error) => LocalError::io(error),
        ignore::Error::Loop { child, .. } => {
            return LocalError::failed("symbolic link loop")
                .operation(Operation::ReadDirectory, Subject::path(child))
                .effects(Effects::OutputIncomplete);
        }
        // The remaining displays embed user patterns.
        _ => LocalError::failed("directory traversal failed"),
    };
    error
        .operation(Operation::ReadDirectory, Subject::path(root))
        .effects(Effects::OutputIncomplete)
}

fn capture_error(error: std::io::Error, operation: Operation) -> LocalError {
    LocalError::io(error)
        .operation(operation, Subject::Label("result capture".into()))
        .effects(Effects::OutputIncomplete)
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

/// Why a file's search stopped. Cancellation is never inferred from text or from a
/// token observed later than an unrelated read failure; a read failure only skips the
/// file.
#[derive(Debug)]
enum SearchStop {
    Cancelled,
    Read,
}

impl grep_searcher::SinkError for SearchStop {
    fn error_message<T: std::fmt::Display>(_message: T) -> Self {
        Self::Read
    }
    fn error_io(_error: std::io::Error) -> Self {
        Self::Read
    }
}

/// Renders one file's matches as its group, `"path": ["line: text", ...`, left open.
struct SearchSink<'a> {
    path: &'a Path,
    workspace: &'a Path,
    group: &'a mut Vec<u8>,
    matcher: &'a grep_regex::RegexMatcher,
    binary: bool,
    cancellation: &'a tokio_util::sync::CancellationToken,
}

impl Sink for SearchSink<'_> {
    type Error = SearchStop;
    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.cancellation.is_cancelled() {
            return Err(SearchStop::Cancelled);
        }
        let line = matched.line_number().unwrap_or(1);
        let bytes = matched.bytes();
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let shown = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        // Only a line cut to a window needs its match placed. The match is found in the
        // line as searched, before its `\r` is dropped for display, so a match on the
        // `\r` still places the window.
        let found = (shown.len() > 4 * MATCH_CONTEXT_BYTES)
            .then(|| {
                grep_matcher::Matcher::find(self.matcher, bytes)
                    .ok()
                    .flatten()
            })
            .flatten()
            .map(|found| found.start()..found.end());
        if self.group.is_empty() {
            let path = relative_path(self.workspace, self.path);
            render(self.group, &path);
            self.group.extend_from_slice(b": [\n");
        } else {
            self.group.extend_from_slice(b",\n");
        }
        render(
            self.group,
            &format_args!("{line}: {}", excerpt(shown, found)),
        );
        Ok(true)
    }
    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

/// Append `text` to `out` as a JSON string.
fn render(out: &mut Vec<u8>, text: &impl Serialize) {
    serde_json::to_writer(out, text).expect("text serializes into memory");
}

/// `line` whole when it is short; otherwise the window around `found`, its match,
/// marked with how long the line was. Neither outgrows the short-line size or the source
/// bytes it shows, however malformed they are.
pub fn excerpt(line: &[u8], found: Option<std::ops::Range<usize>>) -> String {
    if line.len() <= 4 * MATCH_CONTEXT_BYTES {
        return shown(line);
    }
    let (start, end) = found.map_or((0, 0), |found| (found.start, found.end));
    let start = start.min(line.len());
    let end = end.min(start + 2 * MATCH_CONTEXT_BYTES).min(line.len());
    // Cut between characters: a boundary byte is not a UTF-8 continuation byte, looking at
    // most a character's length away, so malformed input cannot widen the window.
    let continuation = |at: usize| line.get(at).is_some_and(|byte| byte & 0xC0 == 0x80);
    let mut from = start.saturating_sub(MATCH_CONTEXT_BYTES);
    for _ in 0..3 {
        if from > 0 && continuation(from) {
            from -= 1;
        }
    }
    let mut to = (end + MATCH_CONTEXT_BYTES).min(line.len());
    for _ in 0..3 {
        if to < line.len() && continuation(to) {
            to += 1;
        }
    }
    format!(
        "{}{}{} (line truncated, {} bytes)",
        if from > 0 { "…" } else { "" },
        shown(&line[from..to]),
        if to < line.len() { "…" } else { "" },
        line.len()
    )
}

/// `bytes` as text, invalid sequences replaced. A replacement character is longer than
/// the byte it stands for, so the text is cut to the larger of the source's length and the
/// short-line size.
fn shown(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    text.truncate(text.floor_char_boundary(bytes.len().max(4 * MATCH_CONTEXT_BYTES)));
    text
}

#[derive(Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Case {
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    /// Rust-regex pattern, using ripgrep syntax
    pattern: String,
    /// File or directory to search
    #[serde(default = "super::default_dot")]
    path: String,
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

#[derive(Debug, Serialize, JsonSchema)]
struct Skipped {
    path: String,
    reason: SkipReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SkipReason {
    /// A directory entry could not be listed
    Unlisted,
    /// The file could not be opened
    Unopened,
    /// Reading stopped partway, including at a line beyond the memory guard
    Unread,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    /// Ripgrep-style file glob, such as `src/**/*.rs`
    pattern: String,
    /// Directory to enumerate
    #[serde(default = "super::default_dot")]
    path: String,
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
    use crate::tool::ToolRegistryBuilder;
    use crate::tool::diagnostic::{Cause, IoKind};
    use crate::tool::output::OutputSink;
    use crate::tool::output::tests::{CaptureStage, FailingCapture};
    use serde_json::{Value, json};
    use std::sync::Arc;

    struct CaptureFixture {
        runtime: tokio::runtime::Runtime,
        sink: Arc<crate::tool::invocation::tests::CapturedOutput>,
        output: crate::tool::output::OutputContext,
    }

    impl CaptureFixture {
        fn new() -> Self {
            Self::with_sink(|sink| sink)
        }

        fn with_sink(
            wrap: impl FnOnce(
                Arc<crate::tool::invocation::tests::CapturedOutput>,
            ) -> Arc<dyn OutputSink>,
        ) -> Self {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let sink = Arc::new(crate::tool::invocation::tests::CapturedOutput::default());
            let output = crate::tool::output::OutputContext::new(wrap(sink.clone()));
            Self {
                runtime,
                sink,
                output,
            }
        }

        fn settle(&self) -> std::io::Result<()> {
            self.runtime
                .block_on(crate::tests::bounded(self.output.settle()))
        }

        fn pending(&self) -> PendingOutput {
            self.runtime
                .block_on(self.output.pending_stream_capture(
                    crate::tool::output::FieldPointer::result().property("test"),
                    crate::tool::output::CaptureKind::Json,
                ))
                .unwrap()
        }
    }

    /// Runs a blocking walker into a fresh capture and decodes what it wrote.
    fn captured<T: serde::de::DeserializeOwned, R>(
        walk: impl FnOnce(PendingOutput, &tokio_util::sync::CancellationToken) -> Result<R, LocalError>,
    ) -> Result<T, LocalError> {
        let fixture = CaptureFixture::new();
        walk(
            fixture.pending(),
            &tokio_util::sync::CancellationToken::new(),
        )?;
        fixture.settle()?;
        Ok(serde_json::from_slice(&fixture.sink.bytes())?)
    }

    fn search(root: &Path, path: &Path, args: Value) -> BTreeMap<String, Vec<String>> {
        let args: SearchArgs = serde_json::from_value(args).unwrap();
        captured(|capture, cancel| search_blocking(root, path, &args, capture, cancel)).unwrap()
    }

    fn glob(root: &Path, path: &Path, args: Value) -> Vec<String> {
        let args: GlobArgs = serde_json::from_value(args).unwrap();
        captured(|capture, cancel| glob_blocking(root, path, &args, capture, cancel)).unwrap()
    }

    fn write_files(root: &Path, files: &[&str]) {
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "needle\n").unwrap();
        }
    }

    #[test]
    fn root_metadata_failures_preserve_io_causes() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        std::fs::write(&input, "needle").unwrap();
        let search: SearchArgs = serde_json::from_value(json!({"pattern":"needle"})).unwrap();
        let glob: GlobArgs = serde_json::from_value(json!({"pattern":"*"})).unwrap();
        for (path, kind) in [
            (root.path().join("missing"), std::io::ErrorKind::NotFound),
            (input.join("child"), std::io::ErrorKind::NotADirectory),
        ] {
            let fixture = CaptureFixture::new();
            let cancel = tokio_util::sync::CancellationToken::new();
            for result in [
                search_blocking(root.path(), &path, &search, fixture.pending(), &cancel)
                    .map(|_| ()),
                glob_blocking(root.path(), &path, &glob, fixture.pending(), &cancel).map(|_| ()),
            ] {
                let error = result.unwrap_err();
                let diagnostic = error.diagnostic();
                assert_eq!(diagnostic.context.operation, Operation::Inspect);
                assert_eq!(diagnostic.context.subject, Subject::path(&path));
                assert_matches!(diagnostic.cause,
                    Cause::Io { kind: actual, code: Some(_), .. } if actual == kind.into());
            }
        }
    }

    #[test]
    fn invalid_patterns_identify_arguments_without_echoing_values() {
        let root = tempfile::tempdir().unwrap();
        let fixture = CaptureFixture::new();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let invalid = "private-pattern-[z-a]";
        let search = |arguments: Value| {
            let args: SearchArgs = serde_json::from_value(arguments).unwrap();
            search_blocking(
                root.path(),
                root.path(),
                &args,
                fixture.pending(),
                &cancellation,
            )
            .unwrap_err()
        };
        let glob: GlobArgs = serde_json::from_value(json!({"pattern":invalid})).unwrap();
        for (error, argument) in [
            (search(json!({"pattern":invalid})), "/pattern"),
            (
                search(json!({"pattern":"needle", "glob":["*.rs", invalid]})),
                "/glob/1",
            ),
            (
                glob_blocking(
                    root.path(),
                    root.path(),
                    &glob,
                    fixture.pending(),
                    &cancellation,
                )
                .unwrap_err(),
                "/pattern",
            ),
        ] {
            let diagnostic = error.diagnostic();
            assert_matches!(diagnostic.cause, Cause::InvalidArguments(_));
            assert_eq!(diagnostic.context.operation, Operation::Validate);
            assert_eq!(
                diagnostic.context.subject,
                Subject::Argument(argument.into())
            );
            // Upstream compile errors echo the pattern; ours must not.
            assert!(!error.to_string().contains("private-pattern"));
        }
    }

    #[test]
    fn walk_failures_preserve_paths_and_io_causes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("missing");
        let source = || std::fs::read(&path).unwrap_err();
        let walked = walk_error(
            ignore::Error::WithDepth {
                depth: 1,
                err: Box::new(ignore::Error::WithPath {
                    path: path.clone(),
                    err: Box::new(ignore::Error::Io(source())),
                }),
            },
            root.path(),
        );
        let diagnostic = walked.diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::ReadDirectory);
        assert_eq!(diagnostic.context.subject, Subject::path(&path));
        assert_eq!(diagnostic.context.effects, Effects::OutputIncomplete);
        assert_eq!(diagnostic.cause, Cause::io(&source()));
    }

    #[test]
    fn capture_failures_preserve_their_phase() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        std::fs::write(&input, "needle\n".repeat(20_000)).unwrap();
        let args: SearchArgs = serde_json::from_value(json!({"pattern":"needle"})).unwrap();
        for (stage, failure, skip) in [
            (CaptureStage::Write, Operation::WriteCapture, 1),
            (CaptureStage::Finish, Operation::FinishCapture, 0),
        ] {
            let fixture = CaptureFixture::with_sink(|captured| {
                Arc::new(FailingCapture::new(captured, stage, skip, || {
                    std::io::Error::from(std::io::ErrorKind::PermissionDenied)
                }))
            });
            let error = search_blocking(
                root.path(),
                &input,
                &args,
                fixture.pending(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .unwrap_err();
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.operation, failure);
            assert_eq!(
                diagnostic.context.subject,
                Subject::Label("result capture".into())
            );
            assert_eq!(diagnostic.context.effects, Effects::OutputIncomplete);
            assert_matches!(
                diagnostic.cause,
                Cause::Io {
                    kind: IoKind::PermissionDenied,
                    ..
                }
            );
            fixture.settle().unwrap();
        }
    }

    /// A cancelled search stops before its first file, and before a walk that only meets
    /// entries it cannot list.
    #[test]
    fn cancellation_before_first_file_is_typed() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        std::fs::write(&input, "needle").unwrap();
        let closed = root.path().join("closed");
        std::fs::create_dir(&closed).unwrap();
        let mode = |mode| {
            std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        mode(0o000);
        let args: SearchArgs = serde_json::from_value(json!({"pattern":"needle"})).unwrap();
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        let results = [&input, &closed].map(|path| {
            let output = CaptureFixture::new();
            search_blocking(root.path(), path, &args, output.pending(), &cancellation)
        });
        mode(0o700);
        for result in results {
            let diagnostic = result.unwrap_err().diagnostic();
            assert_eq!(diagnostic.cause, Cause::Cancelled);
            assert_eq!(diagnostic.context.effects, Effects::OutputIncomplete);
        }
    }

    #[tokio::test]
    async fn grouped_search_is_captured_and_preserves_whitespace_and_drops_binary_files() {
        let runtime = crate::tests::TestRuntime::new().await;
        let directory = runtime.root.path().join("files");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("a.txt"), "  needle  \nneedle again\n").unwrap();
        std::fs::write(directory.join("b.bin"), b"needle\n\0hidden").unwrap();
        std::fs::write(directory.join("z.txt"), "needle last\n").unwrap();
        let mut builder = ToolRegistryBuilder::default();
        builder.register_local(register).unwrap();
        let executor = runtime.executor(builder);
        let search = async |args| {
            executor
                .run_host(&runtime.agent, "search", args)
                .await
                .unwrap()
        };
        let captured = search(json!({"path":"files","pattern":"needle"})).await;
        let grouped = json!({"matches":{
            "files/a.txt":["1:   needle  ", "2: needle again"], "files/z.txt":["1: needle last"]
        }});
        assert_eq!(captured.output.value, grouped);
        let empty = search(json!({"path":"files","pattern":"absent"})).await;
        assert_eq!(empty.output.value, json!({"matches":{}}));
    }

    /// A file or directory the search cannot read is listed and skipped; the other
    /// files' matches stay.
    #[tokio::test]
    async fn unreadable_entries_are_skipped() {
        use std::os::unix::fs::PermissionsExt as _;
        let runtime = crate::tests::TestRuntime::new().await;
        let directory = runtime.root.path().join("files");
        std::fs::create_dir_all(directory.join("closed")).unwrap();
        std::fs::write(directory.join("closed/inner.txt"), "needle\n").unwrap();
        std::fs::write(directory.join("locked.txt"), "needle\n").unwrap();
        std::fs::write(directory.join("open.txt"), "needle\n").unwrap();
        let mode = |path: &str, mode| {
            std::fs::set_permissions(directory.join(path), std::fs::Permissions::from_mode(mode))
                .unwrap();
        };
        mode("closed", 0o000);
        mode("locked.txt", 0o000);
        let mut builder = ToolRegistryBuilder::default();
        builder.register_local(register).unwrap();
        let executor = runtime.executor(builder);
        let searched = executor
            .run_host(
                &runtime.agent,
                "search",
                json!({"path":"files","pattern":"needle"}),
            )
            .await;
        mode("closed", 0o700);
        let value = searched.unwrap().output.value;
        assert_eq!(value["matches"]["files/open.txt"], json!(["1: needle"]));
        assert_eq!(
            value["skipped"],
            json!([
                {"path":"files/closed","reason":"unlisted"},
                {"path":"files/locked.txt","reason":"unopened"},
            ])
        );
    }

    /// A long line's window is placed by the match as searched, even one on its `\r`,
    /// and cut only between characters.
    #[test]
    fn long_line_windows_follow_the_match_and_character_boundaries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("crlf.txt"),
            format!("{}end\r\n", "x".repeat(2000)),
        )
        .unwrap();
        // The seven-byte match ends where the window's end lands inside a character.
        std::fs::write(
            root.path().join("wide.txt"),
            format!("{}needles{}\n", "é".repeat(1000), "é".repeat(1000)),
        )
        .unwrap();
        let found = search(
            root.path(),
            root.path(),
            json!({"pattern":"end\\r$|needles"}),
        );
        let [crlf] = found["crlf.txt"].as_slice() else {
            panic!("{found:?}");
        };
        assert!(
            crlf.starts_with("1: …") && crlf.ends_with("end (line truncated, 2003 bytes)"),
            "{crlf}"
        );
        let [wide] = found["wide.txt"].as_slice() else {
            panic!("{found:?}");
        };
        assert!(
            wide.contains("needles") && !wide.contains('\u{FFFD}'),
            "{wide}"
        );
        // Malformed bytes neither widen the window nor grow it in replacement characters.
        let mut malformed = vec![0x80; 5000];
        malformed.extend_from_slice(b"needles");
        malformed.extend(std::iter::repeat_n(0x80, 5000));
        malformed.push(b'\n');
        std::fs::write(root.path().join("malformed.txt"), malformed).unwrap();
        let found = search(root.path(), root.path(), json!({"pattern":"needles"}));
        let [malformed] = found["malformed.txt"].as_slice() else {
            panic!("{found:?}");
        };
        assert!(malformed.contains("needles"), "{malformed}");
        assert!(
            malformed.len() <= 5 * MATCH_CONTEXT_BYTES,
            "{}",
            malformed.len()
        );
    }

    #[test]
    fn wildcard_filters_respect_hidden_and_ignore_flags() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_files(
            root,
            &[
                ".git/config",
                ".hidden/secret",
                "build/generated",
                "src/visible",
            ],
        );
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        // One case per flag, and per pattern form.
        for (hidden, no_ignore, pattern) in [
            (false, false, "*"),
            (true, false, "**/*"),
            (false, true, "*"),
        ] {
            let args = json!({"pattern":pattern, "hidden":hidden, "no_ignore":no_ignore});
            let paths = glob(root, root, args);
            let has = |path: &str| paths.iter().any(|p| p == path);
            assert!(has("src/visible"));
            assert_eq!(
                (has(".git/config"), has(".hidden/secret")),
                (hidden, hidden)
            );
            assert_eq!(has("build/generated"), no_ignore);
            let args = json!({
                "pattern":"needle", "glob":[pattern], "hidden":hidden, "no_ignore":no_ignore
            });
            let matches = search(root, root, args);
            let has = |path: &str| matches.contains_key(path);
            assert!(has("src/visible"));
            assert_eq!(
                (has(".git/config"), has("build/generated")),
                (hidden, no_ignore)
            );
        }
        // An explicitly requested hidden root remains accessible.
        let paths = glob(
            root,
            &root.join(".git"),
            json!({"pattern":"*", "path":".git"}),
        );
        assert_eq!(paths, [".git/config"]);
    }

    /// A hidden path the caller names, as a root or in a pattern's literal segments, is
    /// walked without `hidden`; hidden entries nothing names stay skipped.
    #[test]
    fn named_hidden_paths_are_walked_without_the_hidden_flag() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_files(
            root,
            &[".config/a.yaml", ".config/.b.yaml", "src/.cache/c", "src/d"],
        );
        let config = root.join(".config");
        let in_config = glob(root, &config, json!({"pattern":"*.yaml", "path":".config"}));
        assert_eq!(in_config, [".config/a.yaml"]);
        let matches = search(root, &config, json!({"pattern":"needle", "path":".config"}));
        assert_eq!(matches.keys().collect::<Vec<_>>(), [".config/a.yaml"]);
        assert_eq!(
            glob(root, root, json!({"pattern":".config/*.yaml"})),
            [".config/a.yaml"]
        );
        let matches = search(
            root,
            root,
            json!({"pattern":"needle", "glob":["src/.cache/*"]}),
        );
        assert_eq!(matches.keys().collect::<Vec<_>>(), ["src/.cache/c"]);
        assert_eq!(glob(root, root, json!({"pattern":"**/*"})), ["src/d"]);
    }

    #[test]
    fn subdirectory_roots_inherit_ignores_and_take_relative_globs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        write_files(
            root,
            &[
                "src/nested/file.rs",
                "src/a.rs",
                "src/b.rs",
                "src/generated.rs",
                "src/skip.rs",
            ],
        );
        std::fs::write(root.join(".gitignore"), "/src/generated.rs\n").unwrap();
        std::fs::write(root.join(".ignore"), "skip.rs\n").unwrap();
        let src = root.join("src");
        let paths = glob(root, &src, json!({"pattern":"*.rs", "path":"src"}));
        assert_eq!(paths, ["src/a.rs", "src/b.rs", "src/nested/file.rs"]);
        // Globs are relative to the requested root.
        let args = json!({"pattern":"needle", "path":"src", "glob":["*.rs", "!nested/*.rs"]});
        let matches = search(root, &src, args);
        assert_eq!(matches.keys().collect::<Vec<_>>(), ["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn ripgrep_smart_case_binary_detection_and_sorted_relative_globs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("upper.rs"), "Needle\n").unwrap();
        std::fs::write(root.join("lower.rs"), "needle\n").unwrap();
        std::fs::write(root.join("skip.txt"), "Needle\n").unwrap();
        std::fs::write(root.join("binary.rs"), b"Needle\0hidden\n").unwrap();
        let args = json!({"pattern":"Needle", "glob":["*.rs", "!binary.rs"], "case":"smart"});
        let output = search(root, root, args);
        assert_eq!(
            output,
            BTreeMap::from([("upper.rs".into(), vec!["1: Needle".into()])])
        );
        let paths = glob(root, root, json!({"pattern":"*.rs"}));
        assert_eq!(paths, ["binary.rs", "lower.rs", "upper.rs"]);
        assert!(glob(root, root, json!({"pattern":"*.missing"})).is_empty());
    }

    /// Whether another description could take a write lock on SQLite's first-opener byte
    /// of the `-shm` behind `probe`, which every live connection holds shared. The probe
    /// stays open across checks: closing it would release those locks itself.
    fn shm_locked(probe: &std::fs::File) -> bool {
        use std::os::fd::AsRawFd as _;
        // SAFETY: an all-zero flock is a valid value of the C struct.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 128;
        lock.l_len = 1;
        // SAFETY: the descriptor is open and `lock` outlives the call.
        assert_eq!(
            unsafe { libc::fcntl(probe.as_raw_fd(), libc::F_OFD_GETLK, &mut lock) },
            0
        );
        i32::from(lock.l_type) != libc::F_UNLCK
    }

    /// Opening and closing a live database's file would release SQLite's locks, which
    /// another process could then take as the database being unused: walks skip its
    /// files and a search of one is refused, both without opening them.
    #[test]
    fn searches_skip_live_databases_and_leave_their_locks() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".skyhook/live.db");
        write_files(root.path(), &[".skyhook/notes"]);
        let db = crate::db::Db::connect(&path, crate::db::OpenMode::Create).unwrap();
        db.query_row("PRAGMA journal_mode = WAL", Vec::new(), |_| Ok(()))
            .unwrap();
        db.batch("CREATE TABLE t(x TEXT); INSERT INTO t VALUES ('needle');")
            .unwrap();
        let shm = root.path().join(".skyhook/live.db-shm");
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&shm)
            .unwrap();
        assert!(shm_locked(&probe));
        // A temporary directory inside a checkout inherits its ignores, which may cover `.skyhook/`.
        let found = search(
            root.path(),
            root.path(),
            json!({"pattern":"needle","hidden":true,"no_ignore":true}),
        );
        assert_eq!(found.keys().collect::<Vec<_>>(), [".skyhook/notes"]);
        let args: SearchArgs = serde_json::from_value(json!({"pattern":"needle"})).unwrap();
        let fixture = CaptureFixture::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let refused = search_blocking(root.path(), &shm, &args, fixture.pending(), &cancel);
        assert!(refused.unwrap_err().to_string().contains("holds open"));
        assert!(shm_locked(&probe));
    }
}
