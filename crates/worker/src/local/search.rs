//! Searching and enumerating this machine's files with ripgrep's matcher, globs and
//! ignore rules.
use std::{
    path::Path,
    sync::{Mutex, OnceLock, PoisonError},
};

use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, overrides::OverrideBuilder};

use crate::diagnostic::{Effects, Failure, Fault, Operation, Subject};
use crate::op::Case;
use crate::op::{
    Events, Glob, MATCH_CONTEXT_BYTES, MatchLine, Search, SearchEvent, SkipReason, Skipped,
    TargetPath, excerpt, line_matcher,
};

impl crate::op::Operation for Search {
    type Event = SearchEvent;
    /// The files the search could not cover, sorted by path.
    type End = Vec<Skipped>;
    type Error = Failure;

    fn paths(&mut self) -> Vec<&mut TargetPath> {
        vec![&mut self.root]
    }

    async fn perform(
        self,
        _: &super::Local,
        events: Events<SearchEvent>,
    ) -> Result<Vec<Skipped>, Failure> {
        skyhook_base::blocking(move || search_blocking(&self, &events)).await
    }
}

impl crate::op::Operation for Glob {
    /// Matched files' paths, in order.
    type Event = Vec<String>;
    type End = ();
    type Error = Failure;

    fn paths(&mut self) -> Vec<&mut TargetPath> {
        vec![&mut self.root]
    }

    async fn perform(self, _: &super::Local, events: Events<Vec<String>>) -> Result<(), Failure> {
        skyhook_base::blocking(move || glob_blocking(&self, &events)).await
    }
}

/// `path` as shown relative to `workspace`, or whole outside it.
pub(crate) fn relative_path(workspace: &Path, path: &Path) -> String {
    match path.strip_prefix(workspace) {
        Ok(path) => {
            let text = path.to_string_lossy();
            match text.is_empty() {
                true => ".".to_owned(),
                false => text.into_owned(),
            }
        }
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

fn walk_builder<'a>(
    root: &Path,
    hidden: bool,
    no_ignore: bool,
    patterns: impl IntoIterator<Item = &'a str>,
) -> Result<WalkBuilder, Option<usize>> {
    let patterns: Vec<_> = patterns.into_iter().collect();
    let named = Named::new(patterns.iter().copied());
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

/// The patterns as overrides, or which one could not be compiled; none names the set.
fn overrides(
    root: &Path,
    patterns: Vec<&str>,
) -> Result<ignore::overrides::Override, Option<usize>> {
    let mut builder = OverrideBuilder::new(root);
    for (index, pattern) in patterns.into_iter().enumerate() {
        builder.add(pattern).map_err(|_| Some(index))?;
    }
    builder.build().map_err(|_| None)
}

fn search_blocking(args: &Search, events: &Events<SearchEvent>) -> Result<Vec<Skipped>, Failure> {
    let on_root = |operation, fault| Failure::at(operation, Subject::path(&args.root), fault);
    let (workspace, root) = (args.workspace.native(), args.root.native());
    let mut matcher_builder = line_matcher();
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
    let matcher = (matcher_builder.build(&args.pattern)).map_err(|_| Fault::Regex)?;
    let metadata =
        std::fs::metadata(root).map_err(|error| on_root(Operation::Inspect, Fault::from(error)))?;
    let walk = if metadata.is_file() {
        None
    } else if metadata.is_dir() {
        let patterns = args.globs.iter().map(String::as_str);
        Some(walk_builder(root, args.hidden, args.no_ignore, patterns).map_err(Fault::Globs)?)
    } else {
        return Err(on_root(Operation::Inspect, Fault::SearchRoot));
    };
    let search = Walk {
        workspace,
        root,
        matcher,
        searcher: {
            let mut searcher = SearcherBuilder::new();
            searcher
                .line_number(true)
                .binary_detection(BinaryDetection::quit(b'\0'))
                .heap_limit(Some(skyhook_base::limits::WHOLE_VALUE_BYTES));
            searcher
        },
        events,
        skipped: Mutex::default(),
        live: OnceLock::new(),
    };
    match walk {
        None => search.worker().file(root, true),
        // Each file is searched whole by one worker, which sends its matches in parts,
        // so files finish in any order and their parts interleave.
        Some(walk) => walk.build_parallel().run(|| {
            let mut worker = search.worker();
            Box::new(move |entry| worker.visit(entry))
        }),
    }
    let Walk { skipped, live, .. } = search;
    if live.into_inner().is_some() {
        return Err(on_root(Operation::Read, Fault::Live));
    }
    let mut skipped = skipped.into_inner().unwrap_or_else(PoisonError::into_inner);
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(skipped)
}

/// One search, shared by the threads walking it.
struct Walk<'a> {
    workspace: &'a Path,
    root: &'a Path,
    matcher: grep_regex::RegexMatcher,
    searcher: SearcherBuilder,
    events: &'a Events<SearchEvent>,
    skipped: Mutex<Vec<Skipped>>,
    /// Set when the search's single file is a live database's.
    live: OnceLock<()>,
}

impl Walk<'_> {
    fn worker(&self) -> Worker<'_> {
        Worker {
            search: self,
            matcher: self.matcher.clone(),
            searcher: self.searcher.build(),
        }
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
    search: &'a Walk<'a>,
    matcher: grep_regex::RegexMatcher,
    searcher: Searcher,
}

impl Worker<'_> {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState {
        if self.search.events.is_closed() {
            return ignore::WalkState::Quit;
        }
        match entry {
            Ok(entry) if entry.file_type().is_some_and(|kind| kind.is_file()) => {
                self.file(entry.path(), false);
            }
            Ok(_) => {}
            Err(error) => {
                let path = error_path(&error).unwrap_or(self.search.root);
                self.search.skip(path, SkipReason::Unlisted);
            }
        }
        match self.search.events.is_closed() {
            true => ignore::WalkState::Quit,
            false => ignore::WalkState::Continue,
        }
    }

    /// Search `path`, sending its matches once it was read whole; `single` when it is
    /// the search's root rather than a file the walk found.
    fn file(&mut self, path: &Path, single: bool) {
        let search = self.search;
        // A live database's file is never opened: a walk passes over it, and a search
        // of that one file is refused.
        let file = match crate::fs::open_regular_blocking(path, u64::MAX) {
            Ok(file) => file,
            Err(crate::op::FsError::Live(_)) if single => {
                let _ = search.live.set(());
                return;
            }
            Err(crate::op::FsError::Live(_)) => return,
            Err(_) => return search.skip(path, SkipReason::Unopened),
        };
        let mut sink = SearchSink {
            matcher: &self.matcher,
            lines: Vec::new(),
            binary: false,
            events: search.events,
        };
        let searched = self.searcher.search_file(&self.matcher, &file, &mut sink);
        // A file that stopped partway keeps none of its matches, like a binary one.
        match searched {
            Ok(()) if sink.binary || sink.lines.is_empty() => {}
            Ok(()) => {
                let path = relative_path(search.workspace, path);
                let mut lines = super::Batch::default();
                for line in sink.lines {
                    let text = line.excerpt.text.len();
                    if let Some(part) = lines.push(line, text) {
                        let path = path.clone();
                        let part = SearchEvent::Part { path, lines: part };
                        let _ = search.events.send_blocking(part);
                    }
                }
                let lines = lines.take();
                let _ = (search.events).send_blocking(SearchEvent::File { path, lines });
            }
            Err(SearchStop::Read) => search.skip(path, SkipReason::Unread),
            Err(SearchStop::Cancelled) => {}
        }
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

fn glob_blocking(args: &Glob, events: &Events<Vec<String>>) -> Result<(), Failure> {
    let inspect = |fault| Failure::at(Operation::Inspect, Subject::path(&args.root), fault);
    let (workspace, root) = (args.workspace.native(), args.root.native());
    let metadata = std::fs::metadata(root).map_err(|error| inspect(Fault::from(error)))?;
    if !metadata.is_dir() {
        return Err(inspect(Fault::GlobRoot));
    }
    let pattern = std::iter::once(args.pattern.as_str());
    let mut walk = (walk_builder(root, args.hidden, args.no_ignore, pattern))
        .map_err(|_| Fault::GlobPattern)?;
    walk.sort_by_file_path(std::cmp::Ord::cmp);
    let mut paths = super::Batch::default();
    for entry in walk.build() {
        if events.is_closed() {
            return Ok(());
        }
        let entry = entry.map_err(|error| walk_error(error, root))?;
        if entry.depth() != 0 && entry.file_type().is_some_and(|kind| kind.is_file()) {
            let path = relative_path(workspace, entry.path());
            let text = path.len();
            if let Some(full) = paths.push(path, text) {
                let _ = events.send_blocking(full);
            }
        }
    }
    let rest = paths.take();
    if !rest.is_empty() {
        let _ = events.send_blocking(rest);
    }
    Ok(())
}

fn walk_error(error: ignore::Error, root: &Path) -> Failure {
    let failed = |path: &Path, fault| {
        let path = Subject::Path(TargetPath::new(path.to_string_lossy()));
        Failure::at(Operation::ReadDirectory, path, fault).effects(Effects::OutputIncomplete)
    };
    let fault = match error {
        ignore::Error::WithPath { path, err } => return walk_error(*err, &path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            return walk_error(*err, root);
        }
        ignore::Error::Partial(mut errors) if errors.len() == 1 => {
            return walk_error(errors.remove(0), root);
        }
        ignore::Error::Io(error) => error.into(),
        ignore::Error::Loop { child, .. } => return failed(&child, Fault::Loop),
        // The remaining displays embed user patterns.
        _ => Fault::Traversal,
    };
    failed(root, fault)
}

/// Why a file's search stopped: its consumer left, or a read failed, which only
/// skips the file.
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

/// Collects one file's matched lines, each cut to a window around its match.
struct SearchSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    lines: Vec<MatchLine>,
    binary: bool,
    events: &'a Events<SearchEvent>,
}

impl Sink for SearchSink<'_> {
    type Error = SearchStop;
    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.events.is_closed() {
            return Err(SearchStop::Cancelled);
        }
        let number = matched.line_number().unwrap_or(1);
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
        let excerpt = excerpt(shown, found);
        self.lines.push(MatchLine { number, excerpt });
        Ok(true)
    }
    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{op::OsError, target::Target as _};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    fn flag(args: &Value, name: &str) -> bool {
        args[name].as_bool().unwrap_or(false)
    }

    type Ran<O> = (
        Vec<<O as crate::op::Operation>::Event>,
        Result<<O as crate::op::Operation>::End, <O as crate::op::Operation>::Error>,
    );

    fn run<O: crate::protocol::Wire>(operation: O) -> Ran<O> {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let local = super::super::Local::new(std::path::PathBuf::from("/"));
            let mut running = local.run(operation).await.unwrap();
            let mut events = Vec::new();
            while let Some(event) = running.event().await {
                events.push(event);
            }
            let Ok(end) = running.end().await;
            (events, end)
        })
    }

    type Searched = (Vec<SearchEvent>, Result<Vec<Skipped>, Failure>);

    fn searched(root: &Path, path: &Path, args: &Value) -> Searched {
        let globs = match &args["glob"] {
            Value::Array(globs) => (globs.iter())
                .map(|glob| glob.as_str().unwrap().to_owned())
                .collect(),
            Value::String(glob) => vec![glob.clone()],
            _ => Vec::new(),
        };
        run(Search {
            root: TargetPath::new(path.to_str().unwrap()),
            workspace: TargetPath::new(root.to_str().unwrap()),
            pattern: args["pattern"].as_str().unwrap().to_owned(),
            fixed: flag(args, "fixed"),
            word: flag(args, "word"),
            case: serde_json::from_value(args["case"].clone()).unwrap_or_default(),
            globs,
            hidden: flag(args, "hidden"),
            no_ignore: flag(args, "no_ignore"),
        })
    }

    /// Matches by path.
    fn search(root: &Path, path: &Path, args: Value) -> BTreeMap<String, Vec<String>> {
        let (events, end) = searched(root, path, &args);
        end.unwrap();
        let mut found = BTreeMap::<String, Vec<String>>::new();
        for event in events {
            let (SearchEvent::Part { path, lines } | SearchEvent::File { path, lines }) = event;
            let shown = lines
                .iter()
                .map(|line| format!("{}: {}", line.number, line.excerpt.text));
            found.entry(path).or_default().extend(shown);
        }
        found
    }

    fn globbed(root: &Path, path: &Path, args: &Value) -> (Vec<Vec<String>>, Result<(), Failure>) {
        run(Glob {
            root: TargetPath::new(path.to_str().unwrap()),
            workspace: TargetPath::new(root.to_str().unwrap()),
            pattern: args["pattern"].as_str().unwrap().to_owned(),
            hidden: flag(args, "hidden"),
            no_ignore: flag(args, "no_ignore"),
        })
    }

    fn glob(root: &Path, path: &Path, args: Value) -> Vec<String> {
        let (paths, end) = globbed(root, path, &args);
        end.unwrap();
        paths.into_iter().flatten().collect()
    }

    fn write_files(root: &Path, files: &[&str]) {
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "needle\n").unwrap();
        }
    }

    #[test]
    fn root_metadata_failures_and_invalid_patterns_are_typed() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        std::fs::write(&input, "needle").unwrap();
        for (path, kind) in [
            (root.path().join("missing"), crate::op::IoKind::NotFound),
            (input.join("child"), crate::op::IoKind::NotADirectory),
        ] {
            let inspected = |failure: Option<Failure>| {
                let failure = failure.unwrap();
                assert_eq!(failure.operation.unwrap().0, Operation::Inspect);
                matches!(failure.fault, Fault::Os(OsError { kind: found, code: Some(_), .. }) if found == kind)
            };
            let (_, search) = searched(root.path(), &path, &json!({"pattern":"needle"}));
            assert!(inspected(search.err()));
            let (_, glob) = globbed(root.path(), &path, &json!({"pattern":"*"}));
            assert!(inspected(glob.err()));
        }
        let invalid = "private-pattern-[z-a]";
        let (_, pattern) = searched(root.path(), root.path(), &json!({"pattern":invalid}));
        assert_eq!(pattern.unwrap_err().fault, Fault::Regex);
        let args = json!({"pattern":"needle", "glob":["*.rs", invalid]});
        let (_, glob) = searched(root.path(), root.path(), &args);
        assert_eq!(glob.unwrap_err().fault, Fault::Globs(Some(1)));
        let (_, glob) = globbed(root.path(), root.path(), &json!({"pattern":invalid}));
        assert_eq!(glob.unwrap_err().fault, Fault::GlobPattern);
    }

    #[test]
    fn walk_failures_keep_their_path_and_os_error() {
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
        let Failure {
            operation: Some((Operation::ReadDirectory, Subject::Path(walked))),
            fault: Fault::Os(error),
            ..
        } = walked
        else {
            panic!("{walked:?}");
        };
        assert_eq!(walked.as_str(), path.to_str().unwrap());
        assert_eq!(error, OsError::from(source()));
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
        let args = json!({"pattern":"end\\r$|needles"});
        let (files, end) = searched(root.path(), root.path(), &args);
        end.unwrap();
        let excerpt = |name: &str| {
            let lines = files.iter().find_map(|file| match file {
                SearchEvent::File { path, lines } if path == name => Some(lines),
                _ => None,
            });
            let [line] = lines.unwrap().as_slice() else {
                panic!("{name}");
            };
            line.excerpt.clone()
        };
        let crlf = excerpt("crlf.txt");
        assert!(crlf.cut_before && !crlf.cut_after && crlf.text.ends_with("end"));
        assert_eq!(crlf.length, Some(2003));
        let wide = excerpt("wide.txt");
        assert!(wide.text.contains("needles") && !wide.text.contains('\u{FFFD}'));
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
        let paths = glob(root, &root.join(".git"), json!({"pattern":"*"}));
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
        let in_config = glob(root, &config, json!({"pattern":"*.yaml"}));
        assert_eq!(in_config, [".config/a.yaml"]);
        let matches = search(root, &config, json!({"pattern":"needle"}));
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
        let paths = glob(root, &src, json!({"pattern":"*.rs"}));
        assert_eq!(paths, ["src/a.rs", "src/b.rs", "src/nested/file.rs"]);
        // Globs are relative to the requested root.
        let args = json!({"pattern":"needle", "glob":["*.rs", "!nested/*.rs"]});
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
}
