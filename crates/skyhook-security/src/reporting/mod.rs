//! Findings populated from reporter prose, and the session's `reports/` as a projection of
//! the store. [`populate`] records a root's finding with the prose its reporter wrote around
//! the facts the store holds; [`generate`] renders `reports/` from the store alone: a folder
//! per root flaw whose latest finding is current, holding its report and a folder per test it
//! states, and an index of them, with every other entry removed. Call both on a blocking
//! worker.

mod prose;
mod render;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, PoisonError},
    time::Duration,
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use skyhook::{
    fs::{BeneathError, Directory, RegularFileError},
    identity::AgentId,
    media::BlobDigest,
};

use crate::store::{
    self, ArtifactCopy, ArtifactReference, ArtifactRole, CandidateId, ConfidenceTier,
    CurrentReport, Finding, FindingId, FlawId, PocArtifact, PocAttempt, PocId, ReportInput,
    ReportSnapshot, ReproductionOutcome, Store, Text,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] crate::store::Error),
    #[error("report files: {0}")]
    Io(#[from] io::Error),
    #[error("report serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error(
        "authored reproduction outcome must agree with the current empirical evidence: the headline is {}demonstrated",
        if *demonstrated { "" } else { "not " }
    )]
    Outcome { demonstrated: bool },
    #[error(
        "the session's reports directory is a symbolic link; reports are written only inside the session"
    )]
    LinkedRoot,
    #[error("report work was cancelled")]
    Cancelled,
}

impl From<BeneathError> for Error {
    fn from(error: BeneathError) -> Self {
        match error {
            BeneathError::Symlink => Self::LinkedRoot,
            BeneathError::File(RegularFileError::Io(error)) => Self::Io(error),
            BeneathError::File(error) => Self::Io(io::Error::other(error)),
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

/// A report's `finding.json`: what its pages render.
#[derive(Serialize)]
struct Document<'a> {
    format_version: u32,
    run: skyhook::identity::SessionId,
    finding: FindingId,
    /// The finding's: the highest among its scenarios.
    severity: crate::cvss::Severity,
    snapshot: &'a ReportSnapshot,
    headline: Headline,
    content: &'a ReportInput,
    /// The artifacts of every test the report states, the headline's first.
    pocs: Vec<PocFolder<'a>>,
}

/// The headline scenario's own assessment and confidence. The finding's severity, the
/// highest among its scenarios, may be above this one's.
#[derive(Clone, Copy, Serialize)]
struct Headline {
    candidate: CandidateId,
    severity: crate::cvss::Severity,
    score: f64,
    vector: crate::cvss::Cvss40,
    confidence: ConfidenceTier,
}

/// One stated test's folder beside the report.
#[derive(Serialize)]
struct PocFolder<'a> {
    poc: PocId,
    candidate: CandidateId,
    /// Relative to the report's folder.
    folder: String,
    files: Vec<&'a ArtifactCopy>,
}

#[derive(Serialize)]
struct PocDocument<'a> {
    format_version: u32,
    poc: &'a PocAttempt,
    files: &'a [&'a ArtifactCopy],
}

/// What `reports/index.json` lists for one root's current report.
#[derive(Serialize)]
struct IndexEntry<'a> {
    root: FlawId,
    finding: FindingId,
    title: &'a Text,
    /// The highest among the finding's scenarios.
    severity: crate::cvss::Severity,
    headline: Headline,
    /// Relative to `reports/`.
    report: String,
}

#[derive(Serialize)]
struct Index<'a> {
    format_version: u32,
    reports: &'a [IndexEntry<'a>],
}

/// A root whose current report could not be written; its folder is left out.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Unwritten {
    pub root: FlawId,
    pub error: String,
}

/// What `reports/` holds after [`generate`].
#[derive(Debug, Default, Serialize, schemars::JsonSchema)]
pub struct Generated {
    /// Every root whose current report is on disk.
    pub reports: Vec<FlawId>,
    pub unwritten: Vec<Unwritten>,
}

const INDEX_MD: &str = "index.md";
const INDEX_JSON: &str = "index.json";

/// The report roots a generation renders now: generations of one root take turns, so one
/// never removes another's staging folder.
static RENDERING: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
static RENDERED: Condvar = Condvar::new();

/// One generation's turn at a report root, until it is dropped.
struct Turn(PathBuf);

impl Turn {
    /// Wait for `root`'s turn while the caller still wants the work.
    fn take(root: PathBuf, cancelled: &dyn Fn() -> bool) -> Result<Self> {
        let mut busy = RENDERING.lock().unwrap_or_else(PoisonError::into_inner);
        while busy.contains(&root) {
            wanted(cancelled)?;
            // Cancellation raises no signal, so the wait wakes to look for it.
            busy = (RENDERED.wait_timeout(busy, Duration::from_millis(50)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        busy.insert(root.clone());
        Ok(Self(root))
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        (RENDERING.lock().unwrap_or_else(PoisonError::into_inner)).remove(&self.0);
        RENDERED.notify_all();
    }
}

/// Reads `inner` while counting and hashing what passes through.
struct Digesting<'a, R> {
    inner: R,
    hasher: Sha256,
    bytes: u64,
    cancelled: &'a dyn Fn() -> bool,
}

impl<'a, R: io::Read> Digesting<'a, R> {
    fn new(inner: R, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
            cancelled,
        }
    }

    /// Reads at most one byte past the size `copy` recorded: enough to tell a longer file
    /// apart, however much longer it is.
    fn recorded(
        inner: R,
        copy: &ArtifactCopy,
        cancelled: &'a dyn Fn() -> bool,
    ) -> Digesting<'a, io::Take<R>> {
        Digesting::new(
            io::Read::take(inner, copy.bytes.saturating_add(1)),
            cancelled,
        )
    }

    fn finish(self) -> (u64, BlobDigest) {
        (
            self.bytes,
            BlobDigest::from_bytes(self.hasher.finalize().into()),
        )
    }
}

/// Why a chunked read stopped early: the caller's cancellation, reported as
/// [`Error::Cancelled`] once the read returns.
#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
struct Stopped;

impl<R: io::Read> io::Read for Digesting<'_, R> {
    /// Each chunk is read only while the work is wanted, so cancelled work stops between
    /// chunks however long the artifact is.
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            return Err(io::Error::other(Stopped));
        }
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        self.bytes += read as u64;
        Ok(read)
    }
}

/// [`Error::Cancelled`] once the caller no longer wants the work, checked before each step
/// that removes or writes.
fn wanted(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// A read that stopped for cancellation as [`Error::Cancelled`].
fn stopped(error: Error) -> Error {
    match error {
        Error::Io(error) if error.get_ref().is_some_and(|inner| inner.is::<Stopped>()) => {
            Error::Cancelled
        }
        error => error,
    }
}

/// Records the root's finding from the reporter's prose and the current evidence, with the
/// size and digest of each artifact its tests recorded. When the root's latest finding
/// already publishes the current scenarios, the prose replaces its write-up instead, unless
/// it is the same write-up. The store compares the evidence again when it records, and
/// settles a record whose commit is unknown. Nothing is written to the session's files. `cancelled` reports
/// whether the caller still wants the result: cancelled work stops between artifact chunks.
pub fn populate(
    store: &Store,
    actor: &AgentId,
    input: ReportInput,
    cancelled: &dyn Fn() -> bool,
) -> Result<Finding> {
    // The form the store reads back, so a settled commit compares equal.
    let input = input.canonical();
    let root = input.root;
    let generation = store.report_generation(root)?;
    let demonstrated = generation.demonstrated;
    if matches!(
        input.reproduction.outcome,
        ReproductionOutcome::Observed { .. }
    ) != demonstrated
    {
        return Err(Error::Outcome { demonstrated });
    }
    let key = generation.key;
    // A finding the root already publishes at this generation keeps the copies it recorded.
    let copies = match store.finding_of(root, &key)? {
        Some(_) => Vec::new(),
        None => copies(
            &store.artifact_root(),
            &store.report_tests(&generation)?,
            cancelled,
        )
        .map_err(stopped)?,
    };
    wanted(cancelled)?;
    match store.record_finding(actor, &input, &key, &copies) {
        Ok(finding) => Ok(finding),
        Err(cause @ store::Error::Unconfirmed(_)) => match store.recorded_finding(&input, &key) {
            Ok(Some(finding)) => Ok(finding),
            _ => Err(cause.into()),
        },
        Err(cause) => Err(cause.into()),
    }
}

/// Renders `reports/` from the findings the store has committed: a folder per current
/// finding, replaced in one step when it does not already hold exactly what renders, then
/// `index.md` and `index.json` listing them, each rewritten only when it changes. Every
/// other entry goes: superseded, stale and withdrawn roots, folders left by interrupted work
/// and anything else placed there. A root whose folder cannot be written is left out and
/// reported. Run again with nothing changed, it changes nothing on disk. Cancelled work stops
/// before its next removal or write, and between artifact chunks.
pub fn generate(store: &Store, cancelled: &dyn Fn() -> bool) -> Result<Generated> {
    let path = store.report_root();
    let _turn = Turn::take(path.clone(), cancelled)?;
    let current = store.current_reports()?;
    let absent = std::fs::symlink_metadata(&path)
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
    if current.is_empty() && absent {
        return Ok(Generated::default());
    }
    wanted(cancelled)?;
    let reports = Directory::open_or_create(&path)?;
    let roots: BTreeSet<String> = (current.iter())
        .map(|report| report.finding.root.to_string())
        .collect();
    for entry in reports.entries()? {
        let keep = entry == INDEX_MD
            || entry == INDEX_JSON
            || entry.to_str().is_some_and(|name| roots.contains(name));
        if !keep {
            wanted(cancelled)?;
            reports.remove_tree(&entry)?;
        }
    }
    let (run, artifacts) = (store.id(), store.artifact_root());
    let mut generated = Generated::default();
    let mut entries = Vec::new();
    for report in &current {
        let root = report.finding.root;
        let document = document(run, report);
        match project(&reports, &artifacts, &document, cancelled).map_err(stopped) {
            Ok(()) => {
                generated.reports.push(root);
                entries.push(IndexEntry {
                    root,
                    finding: report.finding.id,
                    title: &report.input.title,
                    severity: report.finding.severity,
                    headline: document.headline,
                    report: format!("{root}/README.md"),
                });
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => {
                wanted(cancelled)?;
                match reports.remove_tree(OsStr::new(&root.to_string())) {
                    Err(removal) if removal.kind() != io::ErrorKind::NotFound => {
                        return Err(removal.into());
                    }
                    _ => {}
                }
                generated.unwritten.push(Unwritten {
                    root,
                    error: error.to_string(),
                });
            }
        }
    }
    let mut json = serde_json::to_vec_pretty(&Index {
        format_version: 2,
        reports: &entries,
    })?;
    json.push(b'\n');
    replace_changed(&reports, INDEX_JSON, &json, cancelled)?;
    replace_changed(
        &reports,
        INDEX_MD,
        render::index(&entries).as_bytes(),
        cancelled,
    )?;
    reports.sync()?;
    Ok(generated)
}

fn document(run: skyhook::identity::SessionId, report: &CurrentReport) -> Document<'_> {
    let headline = &report.snapshot.content.headline;
    Document {
        format_version: 6,
        run,
        finding: report.finding.id,
        severity: report.finding.severity,
        snapshot: &report.snapshot,
        headline: Headline {
            candidate: headline.candidate.candidate.id,
            severity: headline.assessment.severity,
            score: headline.assessment.score,
            vector: headline.assessment.vector,
            confidence: report.finding.confidence,
        },
        content: &report.input,
        pocs: stated_tests(&report.snapshot)
            .map(|poc| PocFolder {
                poc: poc.id,
                candidate: poc.candidate,
                folder: format!("poc-{}", poc.id),
                files: (report.copies.iter())
                    .filter(|copy| copy.poc == poc.id)
                    .collect(),
            })
            .collect(),
    }
}

/// A file of a report's folder: rendered text, or an artifact copied from the run.
enum Content<'a> {
    Text(Vec<u8>),
    Artifact(&'a ArtifactCopy),
}

/// What a report's folder holds: each file by its path below it, and each folder below it.
struct Layout<'a> {
    files: Vec<(String, Content<'a>)>,
    folders: BTreeSet<String>,
}

fn layout<'a>(document: &'a Document<'a>) -> Result<Layout<'a>> {
    let mut json = serde_json::to_vec_pretty(document)?;
    json.push(b'\n');
    let text = |path: &str, bytes: Vec<u8>| (path.to_owned(), Content::Text(bytes));
    let mut layout = Layout {
        files: vec![
            text("README.md", render::readme(document).into_bytes()),
            text("repro.md", render::reproduction(document).into_bytes()),
            text("finding.json", json),
        ],
        folders: BTreeSet::new(),
    };
    // `document` lists a folder per stated test, in their order.
    for (folder, poc) in document.pocs.iter().zip(stated_tests(document.snapshot)) {
        let mut json = serde_json::to_vec_pretty(&PocDocument {
            format_version: 1,
            poc,
            files: &folder.files,
        })?;
        json.push(b'\n');
        let sums: String = (folder.files.iter())
            .map(|file| format!("{}  {}\n", file.sha256, file.path))
            .collect();
        layout.folders.insert(folder.folder.clone());
        for file in &folder.files {
            layout
                .folders
                .insert(format!("{}/{}", folder.folder, file.role));
            (layout.files).push((
                format!("{}/{}", folder.folder, file.path),
                Content::Artifact(file),
            ));
        }
        let page = render::poc(document, folder, poc).into_bytes();
        for (name, bytes) in [
            ("poc.json", json),
            ("poc.md", page),
            ("SHA256SUMS", sums.into_bytes()),
        ] {
            (layout.files).push(text(&format!("{}/{name}", folder.folder), bytes));
        }
    }
    Ok(layout)
}

/// `path` below `root`: `f` called with the directory holding its last component and that
/// component, each directory on the way opened without following a link.
fn within<T>(
    root: &Directory,
    path: &str,
    f: impl FnOnce(&Directory, &str) -> Result<T>,
) -> Result<T> {
    match path.split_once('/') {
        None => f(root, path),
        Some((first, rest)) => within(&root.open_dir(first.as_ref())?, rest, f),
    }
}

/// Bring the root's folder in line with what renders: kept when it holds exactly that,
/// otherwise built afresh beside it from the store and the run's artifacts, each artifact
/// refused if it no longer matches what the finding recorded, then swapped in for it in one
/// step. A folder replaced or left half-built goes.
fn project(
    reports: &Directory,
    artifacts: &Path,
    document: &Document,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let name = document.snapshot.content.root.flaw.id.to_string();
    let layout = layout(document)?;
    if let Ok(folder) = reports.open_dir(name.as_ref())
        && holds(&folder, &layout, cancelled)?
    {
        return Ok(());
    }
    let staging = skyhook::fs::staging_name(name.as_ref())?;
    wanted(cancelled)?;
    let built = (|| -> Result<()> {
        let folder = reports.create_dir(staging.as_ref())?;
        for path in &layout.folders {
            within(&folder, path, |parent, name| {
                Ok(parent.create_dir(name.as_ref()).map(drop)?)
            })?;
        }
        for (path, content) in &layout.files {
            wanted(cancelled)?;
            within(&folder, path, |parent, name| {
                match content {
                    Content::Text(bytes) => parent.write_new(name.as_ref(), bytes.as_slice())?,
                    Content::Artifact(copy) => {
                        copy_artifact(parent, name, artifacts, copy, cancelled)?
                    }
                }
                Ok(parent.sync()?)
            })?;
        }
        folder.sync()?;
        wanted(cancelled)?;
        match reports.exchange(staging.as_ref(), name.as_ref()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                reports.rename(&staging, name.as_ref())?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(reports.sync()?)
    })();
    // After a swap the staging name holds the folder it replaced.
    match reports.remove_tree(staging.as_ref()) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => built.and(Err(error.into())),
        _ => built,
    }
}

/// Copy one recorded artifact into `directory` as `name`, refusing it once its content no
/// longer matches the size and digest the finding recorded.
fn copy_artifact(
    directory: &Directory,
    name: &str,
    artifacts: &Path,
    copy: &ArtifactCopy,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let recorded = PocArtifact {
        reference: copy.reference.clone(),
        role: copy.role,
    };
    for mut open in store::admit_artifacts(artifacts, &[recorded])? {
        let mut reader = Digesting::recorded(open.contents()?, copy, cancelled);
        directory.write_new(name.as_ref(), &mut reader)?;
        if reader.finish() != (copy.bytes, copy.sha256) {
            return Err(store::Error::Artifact {
                reference: copy.reference.clone(),
                fault: store::ArtifactFault::Changed,
            }
            .into());
        }
    }
    Ok(())
}

/// Whether `folder` holds exactly what `layout` names, each file as it renders or as the
/// finding recorded it. Entries are checked against the layout as they are listed, and the
/// first that differs ends the check.
fn holds(folder: &Directory, layout: &Layout, cancelled: &dyn Fn() -> bool) -> Result<bool> {
    let files: BTreeMap<&str, &Content> = (layout.files.iter())
        .map(|(path, content)| (path.as_str(), content))
        .collect();
    let mut children = BTreeMap::<&str, usize>::new();
    for path in (layout.folders.iter().map(String::as_str)).chain(files.keys().copied()) {
        let parent = path.rfind('/').map_or("", |slash| &path[..=slash]);
        *children.entry(parent).or_default() += 1;
    }
    let expected = Expected {
        layout,
        files,
        children,
    };
    expected.below(folder, "", cancelled)
}

/// What a report's folder should hold: its folders, its files, and how many entries each
/// folder holds directly, by the path prefix of its entries.
struct Expected<'a> {
    layout: &'a Layout<'a>,
    files: BTreeMap<&'a str, &'a Content<'a>>,
    children: BTreeMap<&'a str, usize>,
}

impl Expected<'_> {
    fn below(
        &self,
        folder: &Directory,
        prefix: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let Ok(entries) = folder.entries() else {
            return Ok(false);
        };
        if entries.len() != self.children.get(prefix).copied().unwrap_or_default() {
            return Ok(false);
        }
        for entry in entries {
            wanted(cancelled)?;
            let Some(name) = entry.to_str() else {
                return Ok(false);
            };
            let path = format!("{prefix}{name}");
            let same = if self.layout.folders.contains(&path) {
                match folder.open_dir(name.as_ref()) {
                    Ok(child) => self.below(&child, &format!("{path}/"), cancelled)?,
                    Err(_) => false,
                }
            } else if let Some(content) = self.files.get(path.as_str()) {
                same(folder, name, content, cancelled)?
            } else {
                false
            };
            if !same {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Whether file `name` below `directory` holds `content`: the rendered text, read until it
/// first differs, or the artifact as the finding recorded it.
fn same(
    directory: &Directory,
    name: &str,
    content: &Content,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool> {
    let Ok(file) = directory.open_file(name.as_ref()) else {
        return Ok(false);
    };
    match content {
        Content::Text(bytes) => equal(file, bytes, cancelled),
        Content::Artifact(copy) => {
            let mut digest = Digesting::recorded(file, copy, cancelled);
            match io::copy(&mut digest, &mut io::sink()).map_err(|error| stopped(error.into())) {
                Ok(_) => Ok(digest.finish() == (copy.bytes, copy.sha256)),
                Err(Error::Cancelled) => Err(Error::Cancelled),
                Err(_) => Ok(false),
            }
        }
    }
}

/// Whether `file` holds exactly `expected`, compared chunk by chunk.
fn equal(mut file: impl io::Read, expected: &[u8], cancelled: &dyn Fn() -> bool) -> Result<bool> {
    let mut chunk = [0; 8192];
    let mut at = 0;
    loop {
        wanted(cancelled)?;
        let read = match file.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(false),
        };
        if read == 0 {
            return Ok(at == expected.len());
        }
        if expected.get(at..at + read) != Some(&chunk[..read]) {
            return Ok(false);
        }
        at += read;
    }
}

/// Replace `name` below `directory` with `bytes` unless it already holds them.
fn replace_changed(
    directory: &Directory,
    name: &str,
    bytes: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    if let Ok(file) = directory.open_file(name.as_ref())
        && equal(file, bytes, cancelled)?
    {
        return Ok(());
    }
    wanted(cancelled)?;
    Ok(directory.replace(name.as_ref(), bytes)?)
}

/// Every test the snapshot states, the headline's first.
fn stated_tests(snapshot: &ReportSnapshot) -> impl Iterator<Item = &PocAttempt> {
    let content = &snapshot.content;
    (std::iter::once(&content.headline).chain(&content.variants))
        .filter_map(|scenario| scenario.poc.as_ref())
}

/// The artifacts of every test the report states, each admitted as `record_poc` admitted it
/// and hashed in chunks, named by its role and its own name, numbered on a clash.
fn copies(
    root: &Path,
    tests: &[PocAttempt],
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<ArtifactCopy>> {
    let mut copies = Vec::new();
    for poc in tests {
        let mut taken = BTreeSet::new();
        for (position, mut open) in (store::admit_artifacts(root, &poc.artifacts)?)
            .into_iter()
            .enumerate()
        {
            let mut digest = Digesting::new(open.contents()?, cancelled);
            io::copy(&mut digest, &mut io::sink())?;
            let (bytes, sha256) = digest.finish();
            let artifact = open.artifact;
            copies.push(ArtifactCopy {
                poc: poc.id,
                position: position as u32,
                path: file_path(&mut taken, &artifact.reference, artifact.role),
                reference: artifact.reference,
                role: artifact.role,
                bytes,
                sha256,
            });
        }
    }
    Ok(copies)
}

/// `role/name` for an artifact: its reference's own name, which the reference already keeps
/// to portable characters, with `-2`, `-3`… before the extension when another artifact of the
/// test takes it in any letter case.
fn file_path(
    taken: &mut BTreeSet<String>,
    reference: &ArtifactReference,
    role: ArtifactRole,
) -> String {
    let name =
        (reference.as_str().rsplit('/').next()).expect("validated reference has a final component");
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let mut path = format!("{role}/{name}");
    let mut number = 1;
    while !taken.insert(path.to_ascii_lowercase()) {
        number += 1;
        path = format!("{role}/{stem}-{number}{extension}");
    }
    path
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::symlink, path::PathBuf};

    use super::*;
    use crate::store::{
        ArtifactCitation, Assessor, Confidence, EventKind, FindingStatus, Page, PocArtifact,
        PocNarrative, PocResult, Reproduction, ReproductionStep, ScenarioNote, Section, Subject,
        Verdict,
        tests::{answerer, artifact, confirmed_artifacts, tested, text},
    };

    struct Fixture {
        directory: crate::store::tests::Session,
        store: Store,
        actor: AgentId,
        candidate: crate::store::CandidateId,
        root: FlawId,
    }

    impl Fixture {
        fn new() -> Self {
            let (directory, store, actor) = crate::store::tests::fixture();
            let candidate = crate::store::tests::validated(&store, &actor);
            let root = crate::store::tests::root_of(&store, candidate);
            Self {
                directory,
                store,
                actor,
                candidate,
                root,
            }
        }

        fn input(&self) -> ReportInput {
            ReportInput {
                root: self.root,
                title: text("Unchecked size in the parser"),
                summary: text("An untrusted length reaches an unchecked allocation."),
                analysis: text("The allocation and subsequent copy disagree about the length."),
                impact: text("Memory corruption in the parser process."),
                remediation: text("Validate the length before allocation and copying."),
                reproduction: Reproduction {
                    prerequisites: vec![text("Use a disposable build with the parser enabled.")],
                    steps: vec![ReproductionStep {
                        instruction: text(
                            "Run the supplied payload against the disposable listener.",
                        ),
                        expected_result: text("The instrumented build detects the invalid copy."),
                        artifacts: Vec::new(),
                    }],
                    outcome: ReproductionOutcome::Unobserved {
                        expected_result: text("An instrumented build should detect the overflow."),
                        limitations: text("These proposed steps have not been executed."),
                    },
                    cleanup: vec![text(
                        "Stop only the disposable listener started for this test.",
                    )],
                },
                notes: Default::default(),
                narratives: Default::default(),
                sections: Vec::new(),
            }
        }

        fn observed(&self) -> ReportInput {
            let mut input = self.input();
            input.reproduction.outcome = ReproductionOutcome::Observed {
                observed_result: text("The disposable ASan build reported an out-of-bounds copy."),
            };
            input
        }

        fn poc(
            &self,
            candidate: crate::store::CandidateId,
            result: PocResult,
            artifacts: &[PocArtifact],
        ) -> PocId {
            self.store
                .record_poc(
                    &self.actor,
                    candidate,
                    tested(&self.store, candidate),
                    result,
                    &text("observations from disposable test"),
                    artifacts,
                )
                .unwrap()
                .id
        }

        fn artifacts(&self) -> PathBuf {
            let artifacts = self.store.directory().join("artifacts");
            fs::create_dir_all(&artifacts).unwrap();
            artifacts
        }

        /// Populates the root's finding, then generates `reports/`.
        fn report(&self, input: ReportInput) -> Result<Finding> {
            let finding = populate(&self.store, &self.actor, input, &|| false)?;
            let generated = generate(&self.store, &|| false)?;
            assert!(generated.unwritten.is_empty(), "{generated:?}");
            Ok(finding)
        }

        fn publications(&self) -> usize {
            let events = self
                .store
                .events(Some(Subject::Candidate(self.candidate)), Page::default());
            (events.unwrap().items.iter())
                .filter(|event| event.kind == EventKind::Reported)
                .count()
        }

        fn findings(&self) -> usize {
            self.store
                .findings(Some(self.candidate), Page::default())
                .unwrap()
                .len()
        }

        fn report_root(&self) -> PathBuf {
            self.store.report_root()
        }

        fn folder(&self) -> PathBuf {
            self.report_root().join(self.root.to_string())
        }

        fn document(&self) -> serde_json::Value {
            serde_json::from_slice(&fs::read(self.folder().join("finding.json")).unwrap()).unwrap()
        }

        /// What `reports/` holds, sorted.
        fn listing(&self) -> Vec<String> {
            let mut names: Vec<String> = (fs::read_dir(self.report_root()).unwrap())
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }

        fn indexed(&self) -> Vec<serde_json::Value> {
            let index: serde_json::Value =
                serde_json::from_slice(&fs::read(self.report_root().join("index.json")).unwrap())
                    .unwrap();
            index["reports"].as_array().unwrap().clone()
        }
    }

    #[test]
    fn analytic_report_renders_authored_reproduction_and_computed_cvss() {
        let fixture = Fixture::new();
        let finding = fixture.report(fixture.input()).unwrap();
        assert_eq!(
            (finding.confidence, finding.status),
            (ConfidenceTier::Confirmed, FindingStatus::Current)
        );
        let document = fixture.document();
        assert_eq!(document["headline"]["score"], 9.3);
        assert!(document["pocs"].as_array().unwrap().is_empty());
        let mut files: Vec<_> = (fs::read_dir(fixture.folder()).unwrap())
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["README.md", "finding.json", "repro.md"]);
        let repro = fs::read_to_string(fixture.folder().join("repro.md")).unwrap();
        assert!(repro.contains("Expected outcome (not observed)"));
        // The scenario is rendered from the ledger: each route step with how it is known.
        let readme = fs::read_to_string(fixture.folder().join("README.md")).unwrap();
        assert!(readme.starts_with("---\ntype: finding\n"), "{readme}");
        assert!(readme.contains("entry.c:3"), "{readme}");
        let index = fs::read_to_string(fixture.report_root().join("index.md")).unwrap();
        assert!(
            index.contains(&format!("]({}/README.md)", fixture.root)),
            "{index}"
        );
        assert_eq!(fixture.indexed()[0]["finding"], finding.id.get());
    }

    /// Cancelled population publishes nothing, and a cancelled generation leaves no folder
    /// half-built: the next generation writes it. Cancelled before it starts, generation
    /// removes and writes nothing.
    #[test]
    fn cancelled_work_publishes_nothing_and_leaves_no_folder() {
        let fixture = Fixture::new();
        let cancelled = populate(&fixture.store, &fixture.actor, fixture.input(), &|| true);
        assert!(matches!(cancelled, Err(Error::Cancelled)), "{cancelled:?}");
        assert!(!fixture.report_root().exists());
        let artifacts = confirmed_artifacts(&fixture.store, "payload.bin");
        fixture.poc(fixture.candidate, PocResult::Confirmed, &artifacts);
        // The digest pass reads each artifact and its end; cancellation lands in it.
        let reads = std::cell::Cell::new(0);
        let late = || {
            reads.set(reads.get() + 1);
            reads.get() > 2
        };
        let cancelled = populate(&fixture.store, &fixture.actor, fixture.observed(), &late);
        assert!(matches!(cancelled, Err(Error::Cancelled)), "{cancelled:?}");
        assert_eq!(fixture.findings(), 0);
        populate(&fixture.store, &fixture.actor, fixture.observed(), &|| {
            false
        })
        .unwrap();
        assert!(
            !fixture.report_root().exists(),
            "populating writes no files"
        );
        reads.set(0);
        let cancelled = generate(&fixture.store, &late);
        assert!(matches!(cancelled, Err(Error::Cancelled)), "{cancelled:?}");
        assert_eq!(fixture.listing(), Vec::<String>::new());
        assert_eq!(
            generate(&fixture.store, &|| false).unwrap().reports,
            [fixture.root]
        );
        assert!(fixture.folder().join("finding.json").is_file());

        fs::write(fixture.report_root().join("notes.txt"), "foreign").unwrap();
        fs::remove_file(fixture.report_root().join("index.md")).unwrap();
        fs::remove_dir_all(fixture.folder()).unwrap();
        let cancelled = generate(&fixture.store, &|| true);
        assert!(matches!(cancelled, Err(Error::Cancelled)), "{cancelled:?}");
        assert_eq!(fixture.listing(), ["index.json", "notes.txt"]);
    }

    /// Populating an unchanged generation again with the same write-up records
    /// nothing more.
    #[test]
    fn republishing_an_unchanged_report_records_no_new_publication() {
        let fixture = Fixture::new();
        let first = fixture.report(fixture.input()).unwrap();
        assert_eq!(fixture.publications(), 1);
        let again = fixture.report(fixture.input()).unwrap();
        assert_eq!(
            (again.id, fixture.findings(), fixture.publications()),
            (first.id, 1, 1)
        );
    }

    /// A write-up reads back in the store's form, which settles an unknown commit, and
    /// populating it again in any spelling of that form rewrites nothing.
    #[test]
    fn a_repeated_write_up_is_the_one_recorded() {
        let fixture = Fixture::new();
        let artifacts = confirmed_artifacts(&fixture.store, "payload.bin");
        let poc = fixture.poc(fixture.candidate, PocResult::Confirmed, &artifacts);
        let [attack, control] = artifacts.map(|artifact| ArtifactCitation {
            poc,
            reference: artifact.reference,
        });
        let mut input = fixture.observed();
        input.reproduction.steps[0].artifacts = vec![
            control.clone(),
            attack.clone(),
            control.clone(),
            attack.clone(),
        ];
        let first = populate(&fixture.store, &fixture.actor, input.clone(), &|| false).unwrap();
        let stored = fixture.store.finding(fixture.root).unwrap().write_up;
        assert_eq!(stored.reproduction.steps[0].artifacts, [control, attack]);
        assert_eq!(stored, *input.clone().canonical());
        let again = populate(&fixture.store, &answerer(&fixture.actor), input, &|| false).unwrap();
        assert_eq!((again.id, again.actor), (first.id, first.actor));
    }

    /// Checking a copied artifact and copying it again read no further than one byte past
    /// the size the finding recorded, however far the file has grown.
    #[test]
    fn a_grown_artifact_is_read_only_past_its_recorded_size() {
        let fixture = Fixture::new();
        let artifacts = confirmed_artifacts(&fixture.store, "payload.bin");
        let poc = fixture.poc(fixture.candidate, PocResult::Confirmed, &artifacts);
        fixture.report(fixture.observed()).unwrap();
        let source = fixture.artifacts().join("payload.bin");
        let copied = fixture
            .folder()
            .join(format!("poc-{poc}/attack/payload.bin"));
        let original = fs::read(&source).unwrap();
        let reads = |grown: u64| {
            fs::write(&source, &original).unwrap();
            generate(&fixture.store, &|| false).unwrap();
            for file in [&source, &copied] {
                let file = fs::OpenOptions::new().write(true).open(file).unwrap();
                file.set_len(original.len() as u64 + grown).unwrap();
            }
            let checks = std::cell::Cell::new(0);
            let generated = generate(&fixture.store, &|| {
                checks.set(checks.get() + 1);
                false
            })
            .unwrap();
            assert_eq!(generated.unwritten[0].root, fixture.root);
            checks.get()
        };
        assert_eq!(reads(1), reads(16 << 20));
    }

    /// The store decides what the folder holds: generating again with nothing changed
    /// rewrites nothing, a tampered folder is rendered again from the store, and one whose
    /// artifacts no longer match the finding leaves `reports/`.
    #[test]
    fn a_confirmed_poc_is_copied_by_role_and_the_folder_follows_the_store() {
        let fixture = Fixture::new();
        let store = &fixture.store;
        let poc = fixture.poc(
            fixture.candidate,
            PocResult::Confirmed,
            &[
                artifact(store, "case/payload.bin", ArtifactRole::Attack),
                artifact(store, "other/Payload.bin", ArtifactRole::Attack),
                artifact(store, "observations.txt", ArtifactRole::Control),
            ],
        );
        let first = fixture.report(fixture.observed()).unwrap();
        assert_eq!(first.confidence, ConfidenceTier::Demonstrated);
        let folder = fixture.folder().join(format!("poc-{poc}"));
        for (copy, original) in [
            ("attack/payload.bin", "case/payload.bin"),
            ("attack/Payload-2.bin", "other/Payload.bin"),
        ] {
            assert_eq!(fs::read_to_string(folder.join(copy)).unwrap(), original);
        }
        let digest = BlobDigest::of(b"case/payload.bin");
        let sums = fs::read_to_string(folder.join("SHA256SUMS")).unwrap();
        assert!(
            sums.starts_with(&format!("{digest}  attack/payload.bin\n")),
            "{sums}"
        );
        for page in ["poc.md", "poc.json"] {
            assert!(folder.join(page).is_file(), "{page}");
        }
        let files = &fixture.document()["pocs"][0]["files"];
        assert_eq!(files[1]["path"], "attack/Payload-2.bin");
        assert_eq!(files[0]["bytes"], "case/payload.bin".len());
        assert_eq!(files[0]["sha256"], digest.to_string());

        use std::os::unix::fs::MetadataExt as _;
        let stamp = |path: PathBuf| {
            let metadata = fs::metadata(path).unwrap();
            (metadata.ino(), metadata.mtime_nsec(), metadata.mtime())
        };
        let before = [
            fixture.folder().join("README.md"),
            fixture.report_root().join("index.md"),
        ]
        .map(stamp);
        generate(&fixture.store, &|| false).unwrap();
        let after = [
            fixture.folder().join("README.md"),
            fixture.report_root().join("index.md"),
        ]
        .map(stamp);
        assert_eq!(before, after, "an unchanged generation writes nothing");

        // Identical scenarios rewrite the latest finding's prose across agents; a folder that
        // no longer holds what renders is rendered again.
        fs::write(fixture.folder().join("finding.json"), "edited").unwrap();
        let actor = answerer(&fixture.actor);
        let mut reworded = fixture.observed();
        reworded.summary = text("A reworded summary of the same evidence.");
        let again = populate(&fixture.store, &actor, reworded, &|| false).unwrap();
        generate(&fixture.store, &|| false).unwrap();
        assert_eq!((again.id, fixture.findings()), (first.id, 1));
        let readme = fs::read_to_string(fixture.folder().join("README.md")).unwrap();
        assert!(readme.contains("A reworded summary"), "{readme}");

        // Artifact content that no longer matches the finding leaves nothing behind.
        let artifacts = fixture.artifacts();
        fs::write(artifacts.join("case/payload.bin"), "corrected payload").unwrap();
        fs::write(fixture.folder().join("finding.json"), "edited").unwrap();
        let generated = generate(&fixture.store, &|| false).unwrap();
        assert_eq!(generated.unwritten[0].root, fixture.root);
        assert_eq!(fixture.listing(), ["index.json", "index.md"]);
        assert!(fixture.indexed().is_empty());
    }

    /// A demonstrated headline less severe than a scenario whose test failed: the report
    /// copies the artifacts of every test it states, and its metadata gives the finding's
    /// severity apart from the headline's own severity, score and vector.
    #[test]
    fn a_report_states_every_test_and_the_headline_apart_from_the_finding() {
        const LOCAL: &str = "CVSS:4.0/AV:L/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";
        let fixture = Fixture::new();
        let (store, actor) = (&fixture.store, &fixture.actor);
        fixture.poc(
            fixture.candidate,
            PocResult::Failed,
            &[artifact(store, "crash.log", ArtifactRole::Observation)],
        );
        let headline = crate::store::tests::validated(store, actor);
        let vector: crate::cvss::Cvss40 = LOCAL.parse().unwrap();
        store
            .assess(
                actor,
                headline,
                vector,
                Confidence::Established,
                &text("local only"),
                Assessor::Reviewer,
            )
            .unwrap();
        store
            .record_validation(actor, headline, Verdict::Validated, &text("reviewed"))
            .unwrap();
        let flaw = store.candidate(headline).unwrap().members[0].flaw.id;
        store
            .relate_flaws(
                actor,
                fixture.root,
                flaw,
                crate::store::RelationKind::Enables,
                &text("feeds it"),
            )
            .unwrap();
        let artifacts = confirmed_artifacts(store, "payload.bin");
        fixture.poc(headline, PocResult::Confirmed, &artifacts);
        fixture.report(fixture.observed()).unwrap();
        let mut copied: Vec<String> = (fixture.document()["pocs"].as_array().unwrap().iter())
            .flat_map(|poc| {
                let folder = fixture.folder().join(poc["folder"].as_str().unwrap());
                (poc["files"].as_array().unwrap().iter())
                    .map(move |file| {
                        fs::read_to_string(folder.join(file["path"].as_str().unwrap())).unwrap()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        copied.sort();
        assert_eq!(copied, ["control.log", "crash.log", "payload.bin"]);

        let (severity, score) = (vector.severity(), format!("{:.1}", vector.score()));
        assert_ne!(severity, crate::cvss::Severity::Critical);
        let entry = &fixture.indexed()[0];
        assert_eq!(entry["severity"], "critical");
        assert_eq!(
            entry["headline"],
            serde_json::json!({
                "candidate": headline,
                "severity": severity,
                "score": vector.score(),
                "vector": LOCAL,
                "confidence": "demonstrated",
            })
        );
        let readme = fs::read_to_string(fixture.folder().join("README.md")).unwrap();
        let front = format!(
            "severity: critical\nheadline: {headline}\nheadline_severity: {severity}\n\
             headline_score: {score}\nheadline_vector: {LOCAL:?}\n"
        );
        assert!(readme.contains(&front), "{readme}");
    }

    /// A failed test leaves the scenario reported at `confirmed`, stating the failure.
    #[test]
    fn report_outcome_must_match_the_recorded_empirical_state() {
        for (result, observed, expected) in [
            (
                Some(PocResult::Failed),
                false,
                Some(ConfidenceTier::Confirmed),
            ),
            (Some(PocResult::Failed), true, None),
            (
                Some(PocResult::Pending),
                false,
                Some(ConfidenceTier::Confirmed),
            ),
            (
                Some(PocResult::Declined),
                false,
                Some(ConfidenceTier::Confirmed),
            ),
            (None, true, None),
            (Some(PocResult::Confirmed), false, None),
        ] {
            let fixture = Fixture::new();
            if let Some(result) = result {
                let confirmed = confirmed_artifacts(&fixture.store, "payload.bin");
                fixture.poc(
                    fixture.candidate,
                    result,
                    &confirmed[..2 * (result == PocResult::Confirmed) as usize],
                );
            }
            let input = if observed {
                fixture.observed()
            } else {
                fixture.input()
            };
            let finding = fixture.report(input);
            assert_eq!(
                finding.as_ref().ok().map(|finding| finding.confidence),
                expected,
                "{result:?} observed={observed}"
            );
            match &finding {
                Err(_) => {
                    assert!(!fixture.report_root().exists(), "{result:?}");
                    assert_eq!(fixture.findings(), 0);
                }
                Ok(_) if result == Some(PocResult::Failed) => {
                    let readme = fs::read_to_string(fixture.folder().join("README.md")).unwrap();
                    assert!(readme.contains("not demonstrated: PoC"), "{readme}");
                }
                Ok(_) => {}
            }
        }
    }

    /// References are checked when recorded (a regular file under the run's
    /// artifacts) and again when copied, so a file replaced by a link in between is
    /// still refused.
    #[test]
    fn artifact_references_never_leave_the_run_or_follow_links() {
        let fixture = Fixture::new();
        let external = tempfile::tempdir().unwrap();
        fs::write(external.path().join("payload"), "outside secret").unwrap();
        let artifacts = fixture.artifacts();
        symlink(external.path().join("payload"), artifacts.join("linked")).unwrap();
        let attack = |reference: &str| PocArtifact {
            reference: reference.parse().unwrap(),
            role: ArtifactRole::Attack,
        };
        let control = artifact(&fixture.store, "control.log", ArtifactRole::Control);
        for reference in ["linked", "missing"] {
            assert!(matches!(
                fixture.store.record_poc(
                    &fixture.actor,
                    fixture.candidate,
                    tested(&fixture.store, fixture.candidate),
                    PocResult::Confirmed,
                    &text("observations"),
                    &[attack(reference), control.clone()]
                ),
                Err(crate::store::Error::Artifact { .. })
            ));
        }
        fs::write(artifacts.join("payload"), "payload").unwrap();
        let recorded = [attack("payload"), control];
        fixture.poc(fixture.candidate, PocResult::Confirmed, &recorded);
        fs::remove_file(artifacts.join("payload")).unwrap();
        symlink(external.path().join("payload"), artifacts.join("payload")).unwrap();
        assert!(matches!(
            fixture.report(fixture.observed()),
            Err(Error::Store(crate::store::Error::Artifact {
                fault: crate::store::ArtifactFault::Symlink,
                ..
            }))
        ));
        assert!(!fixture.report_root().exists());
        assert_eq!(fixture.findings(), 0);
    }

    #[test]
    fn a_linked_reports_directory_is_refused() {
        let fixture = Fixture::new();
        let elsewhere = fixture.directory.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, fixture.report_root()).unwrap();
        assert!(matches!(
            fixture.report(fixture.input()),
            Err(Error::LinkedRoot)
        ));
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// `reports/` holds only current reports: a root folder replaced by a link is replaced
    /// without being followed, anything else placed there goes, and a root whose report
    /// went stale leaves until its new report is published, which takes its place.
    #[test]
    fn generation_keeps_reports_to_the_current_findings() {
        let fixture = Fixture::new();
        let first = fixture.report(fixture.input()).unwrap();
        let elsewhere = fixture.store.directory().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::remove_dir_all(fixture.folder()).unwrap();
        symlink(&elsewhere, fixture.folder()).unwrap();
        fs::create_dir(fixture.report_root().join(".7-orphan")).unwrap();
        fs::write(fixture.report_root().join("notes.txt"), "foreign").unwrap();
        generate(&fixture.store, &|| false).unwrap();
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert!(fixture.folder().join("finding.json").is_file());
        assert_eq!(
            fixture.listing(),
            [
                fixture.root.to_string(),
                "index.json".into(),
                "index.md".into()
            ]
        );

        fixture
            .store
            .assess(
                &fixture.actor,
                fixture.candidate,
                "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:L/VI:L/VA:L/SC:N/SI:N/SA:N"
                    .parse()
                    .unwrap(),
                Confidence::Established,
                &text("only partial corruption"),
                Assessor::Reviewer,
            )
            .unwrap();
        fixture
            .store
            .record_validation(
                &fixture.actor,
                fixture.candidate,
                Verdict::Validated,
                &text("reviewed the score"),
            )
            .unwrap();
        let stale = |fixture: &Fixture| {
            fixture.store.findings(None, Page::default()).unwrap().items[0].status
        };
        assert_eq!(stale(&fixture), FindingStatus::Stale);
        generate(&fixture.store, &|| false).unwrap();
        assert_eq!(fixture.listing(), ["index.json", "index.md"]);
        let second = fixture.report(fixture.input()).unwrap();
        assert_eq!(stale(&fixture), FindingStatus::Superseded { by: second.id });
        assert_eq!(fixture.indexed()[0]["finding"], second.id.get());
        assert_ne!(first.id, second.id);
    }

    /// The prose is kept with the finding: notes, narratives and cited artifacts must be of
    /// what the report states, the report renders again from the store alone, and the finding
    /// reads back as it renders.
    #[test]
    fn prose_is_kept_with_the_finding_and_checked_against_what_it_states() {
        let fixture = Fixture::new();
        let artifacts = confirmed_artifacts(&fixture.store, "payload.bin");
        let poc = fixture.poc(fixture.candidate, PocResult::Confirmed, &artifacts);
        let mut input = fixture.observed();
        input.notes.push(ScenarioNote {
            candidate: fixture.candidate,
            note: text("Any request reaches it."),
        });
        input.narratives.push(PocNarrative {
            poc,
            narrative: text("The run crashed on the first request."),
        });
        input.sections.push(Section {
            heading: text("Disclosure"),
            body: text("Reported to the maintainers."),
        });
        input.reproduction.steps[0]
            .artifacts
            .push(ArtifactCitation {
                poc,
                reference: "payload.bin".parse().unwrap(),
            });
        let refused = |change: &dyn Fn(&mut ReportInput)| {
            let mut wrong = input.clone();
            change(&mut wrong);
            populate(&fixture.store, &fixture.actor, wrong, &|| false).unwrap_err()
        };
        let elsewhere = crate::store::tests::shortlisted(&fixture.store, &fixture.actor);
        assert!(matches!(
            refused(&|input| {
                input.notes.push(ScenarioNote {
                    candidate: elsewhere,
                    note: text("not reported"),
                })
            }),
            Error::Store(crate::store::Error::UnreportedScenario { .. })
        ));
        assert!(matches!(
            refused(&|input| input.notes.push(input.notes[0].clone())),
            Error::Store(crate::store::Error::RepeatedProse { .. })
        ));
        assert!(matches!(
            refused(
                &|input| input.reproduction.steps[0].artifacts[0].reference =
                    "control.txt".parse().unwrap()
            ),
            Error::Store(crate::store::Error::UnstatedArtifact { .. })
        ));
        assert_eq!(fixture.findings(), 0);
        assert!(matches!(
            fixture.store.finding(fixture.root),
            Err(crate::store::Error::NotFound {
                record: crate::store::Record::Finding,
                ..
            })
        ));
        fixture.report(input.clone()).unwrap();
        let readme = fs::read_to_string(fixture.folder().join("README.md")).unwrap();
        for expected in [
            "Any request reaches it.",
            "The run crashed on the first request.",
            "Disclosure",
            "Reported to the maintainers.",
        ] {
            assert!(readme.contains(expected), "{expected}: {readme}");
        }
        let repro = fs::read_to_string(fixture.folder().join("repro.md")).unwrap();
        assert!(
            repro.contains(&format!("(poc-{poc}/attack/payload.bin)")),
            "{repro}"
        );
        fs::remove_dir_all(fixture.folder()).unwrap();
        generate(&fixture.store, &|| false).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.folder().join("README.md")).unwrap(),
            readme
        );

        // Read back, the finding holds what its report renders, and keeps it once stale.
        let document = fixture.document();
        let content = &document["snapshot"]["content"];
        let rendered: serde_json::Value = (std::iter::once(&content["headline"]))
            .chain(content["variants"].as_array().unwrap())
            .map(|scenario| {
                serde_json::json!({
                    "candidate": scenario["candidate"]["candidate"]["id"],
                    "assessment": scenario["assessment"],
                    "validation": scenario["validation"],
                    "poc": scenario["poc"],
                })
            })
            .collect();
        let read = fixture.store.finding(fixture.root).unwrap();
        assert_eq!(read.finding.status, FindingStatus::Current);
        assert_eq!(serde_json::to_value(&read.scenarios).unwrap(), rendered);
        assert_eq!(
            serde_json::to_value(&read.write_up).unwrap(),
            document["content"]
        );
        fixture
            .store
            .record_validation(
                &fixture.actor,
                fixture.candidate,
                Verdict::Validated,
                &text("reviewed again"),
            )
            .unwrap();
        let stale = fixture.store.finding(fixture.root).unwrap();
        assert_eq!(stale.finding.status, FindingStatus::Stale);
        assert_eq!(serde_json::to_value(&stale.scenarios).unwrap(), rendered);
        assert_eq!(stale.write_up, input);
    }
}
