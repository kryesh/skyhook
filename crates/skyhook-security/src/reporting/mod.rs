//! The session's `reports/` as a projection of the store: [`generate`] renders a folder per
//! root flaw whose finding reports its scenarios as they stand, holding its report and a
//! folder per test it states with that test's artifacts, and an index of them, with every
//! other entry removed. Call it on a blocking worker.

mod document;
mod folder;
mod prose;
mod render;

use self::{document::*, folder::*};

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, PoisonError},
    time::Duration,
};

use serde::Serialize;
use skyhook::{
    fs::{BeneathError, Directory, RegularFileError},
    media::BlobDigest,
};

use crate::{
    cvss::{Cvss40, Severity},
    store::{
        self, ArtifactFault, ArtifactReference, ArtifactRole, CandidateId, ConfidenceTier,
        CurrentReport, Digesting, FindingId, FlawId, RecordedArtifact, ReportedScenario,
        ReportedTest, Stopped, Store, TestArtifact,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error("report files: {0}")]
    Io(#[from] io::Error),
    #[error("report serialization: {0}")]
    Json(#[from] serde_json::Error),
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

impl From<store::Inadmissible> for Error {
    fn from(error: store::Inadmissible) -> Self {
        Self::Store(error.into())
    }
}

type Result<T> = std::result::Result<T, Error>;

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

/// Renders `reports/` from the findings the store holds: a folder per current one, replaced in
/// one step when it does not already hold exactly what renders, then `index.md` and
/// `index.json` listing them, each rewritten only when it changes. Every other entry goes:
/// stale roots, roots with nothing to report, folders left by interrupted work, an index name
/// held by anything but a file, and anything else placed there. A root whose folder cannot be
/// written, such as one whose artifact changed since its test was recorded, is left out and
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
        .map(|report| report.root.to_string())
        .collect();
    for entry in reports.entries()? {
        let keep = match entry.to_str() {
            Some(INDEX_MD | INDEX_JSON) => reports.open_file(&entry).is_ok(),
            Some(name) => roots.contains(name),
            None => false,
        };
        if !keep {
            wanted(cancelled)?;
            reports.remove_tree(&entry)?;
        }
    }
    let (run, artifacts) = (store.id(), store.artifact_root());
    let mut generated = Generated::default();
    let mut entries = Vec::new();
    for report in &current {
        let root = report.root;
        let document = document(run, report);
        match project(&reports, &artifacts, &document, cancelled).map_err(stopped) {
            Ok(()) => {
                generated.reports.push(root);
                entries.push(IndexEntry {
                    root,
                    finding: report.finding,
                    title: &report.title,
                    severity: document.severity,
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
        format_version: 3,
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

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::symlink};

    use skyhook::identity::AgentId;

    use super::*;
    use crate::store::{
        NewTest, TestId, TestResult, Verdict,
        tests::{Scenario, Session, artifact, fixture, text, validated, verdict},
    };

    struct Fixture {
        _session: Session,
        store: Store,
        actor: AgentId,
        scenario: Scenario,
    }

    impl Fixture {
        /// A validated scenario whose root is reported.
        fn new() -> Self {
            let (session, store, actor) = fixture();
            let scenario = validated(&store, &actor, "parser");
            let fixture = Self {
                _session: session,
                store,
                actor,
                scenario,
            };
            fixture.report("Unchecked size in the parser");
            fixture
        }

        fn report(&self, title: &str) {
            let body = text("## Summary\n\nAn untrusted length reaches an unchecked copy.");
            (self.store)
                .report(&self.actor, self.scenario.flaw, &text(title), &body)
                .unwrap();
        }

        fn test(&self, result: TestResult, artifacts: Vec<crate::store::TestArtifact>) -> TestId {
            let test = NewTest {
                candidate: self.scenario.candidate,
                result,
                note: text("ran the payload twice"),
                environment: None,
                artifacts,
            };
            self.store.record_test(&self.actor, &test).unwrap()
        }

        fn generate(&self) -> Generated {
            generate(&self.store, &|| false).unwrap()
        }

        fn folder(&self) -> PathBuf {
            self.store
                .report_root()
                .join(self.scenario.flaw.to_string())
        }

        fn listing(&self) -> Vec<String> {
            let mut names: Vec<_> = (fs::read_dir(self.store.report_root()).unwrap())
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }
    }

    /// `reports/` holds only current reports: a root folder replaced by a link is replaced
    /// without being followed, anything else placed there goes, and a root whose report went
    /// stale leaves until its report is written again. Run again with nothing changed, it
    /// rewrites nothing.
    #[test]
    fn generation_keeps_reports_to_the_current_findings() {
        let fixture = Fixture::new();
        assert_eq!(fixture.generate().reports, [fixture.scenario.flaw]);
        let readme = fixture.folder().join("README.md");
        let written = fs::metadata(&readme).unwrap().modified().unwrap();
        fixture.generate();
        assert_eq!(fs::metadata(&readme).unwrap().modified().unwrap(), written);

        let elsewhere = fixture.store.directory().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::remove_dir_all(fixture.folder()).unwrap();
        symlink(&elsewhere, fixture.folder()).unwrap();
        fs::write(fixture.store.report_root().join("notes.txt"), "foreign").unwrap();
        fixture.generate();
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert!(fixture.folder().join("finding.json").is_file());
        let root = fixture.scenario.flaw.to_string();
        assert_eq!(fixture.listing(), [root.as_str(), "index.json", "index.md"]);

        let candidate = fixture.scenario.candidate;
        verdict(
            &fixture.store,
            &fixture.actor,
            candidate,
            Verdict::Validated,
        );
        fixture.generate();
        assert_eq!(fixture.listing(), ["index.json", "index.md"]);
        fixture.report("Unchecked size in the parser");
        assert_eq!(fixture.generate().reports, [fixture.scenario.flaw]);
    }

    /// A test's artifacts are copied by role with their checksums, and a confirmation shows
    /// the artifacts it lacks; an artifact changed since its test was recorded leaves its root
    /// unwritten.
    #[test]
    fn a_tests_artifacts_are_copied_only_as_recorded() {
        let fixture = Fixture::new();
        let attack = artifact(&fixture.store, "payload.bin", ArtifactRole::Attack);
        let test = fixture.test(TestResult::Confirmed, vec![attack]);
        fixture.report("Unchecked size in the parser");
        fixture.generate();
        let folder = fixture.folder().join(format!("test-{test}"));
        assert_eq!(
            fs::read(folder.join("attack/payload.bin")).unwrap(),
            b"payload.bin"
        );
        let sums = fs::read_to_string(folder.join("SHA256SUMS")).unwrap();
        assert!(sums.ends_with("  attack/payload.bin\n"), "{sums}");
        let page = fs::read_to_string(folder.join("test.md")).unwrap();
        assert!(page.contains("confirmed, no control artifact"), "{page}");

        fs::write(fixture.store.artifact_root().join("payload.bin"), "changed").unwrap();
        fs::remove_dir_all(fixture.folder()).unwrap();
        let generated = fixture.generate();
        assert_eq!(
            generated
                .unwritten
                .iter()
                .map(|unwritten| unwritten.root)
                .collect::<Vec<_>>(),
            [fixture.scenario.flaw]
        );
        assert!(!fixture.folder().exists());
    }

    /// The index sets each report's title as a link's text whatever it holds.
    #[test]
    fn the_index_escapes_titles() {
        let fixture = Fixture::new();
        fixture.report("Copy [past](http://x) | the end");
        fixture.generate();
        let index = fs::read_to_string(fixture.store.report_root().join(INDEX_MD)).unwrap();
        let root = fixture.scenario.flaw;
        assert!(
            index.contains(&format!(
                "Copy \\[past\\](http://x) \\| the end]({root}/README.md)"
            )),
            "{index}"
        );
    }

    #[test]
    fn a_linked_reports_directory_is_refused() {
        let fixture = Fixture::new();
        let elsewhere = fixture.store.directory().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, fixture.store.report_root()).unwrap();
        assert!(matches!(
            generate(&fixture.store, &|| false),
            Err(Error::LinkedRoot)
        ));
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// A cancelled generation leaves no folder half-built, and the next one writes it;
    /// cancelled before it starts, it removes and writes nothing.
    #[test]
    fn cancelled_generation_leaves_no_folder() {
        let fixture = Fixture::new();
        let attack = artifact(&fixture.store, "payload.bin", ArtifactRole::Attack);
        fixture.test(TestResult::Failed, vec![attack]);
        fixture.report("Unchecked size in the parser");
        let checks = std::cell::Cell::new(0);
        let late = || {
            checks.set(checks.get() + 1);
            checks.get() > 3
        };
        let cancelled = generate(&fixture.store, &late);
        assert!(matches!(cancelled, Err(Error::Cancelled)), "{cancelled:?}");
        assert_eq!(fixture.listing(), Vec::<String>::new());
        assert_eq!(fixture.generate().reports, [fixture.scenario.flaw]);

        fs::write(fixture.store.report_root().join("notes.txt"), "foreign").unwrap();
        let cancelled = generate(&fixture.store, &|| true);
        assert!(matches!(cancelled, Err(Error::Cancelled)), "{cancelled:?}");
        assert!(fixture.store.report_root().join("notes.txt").exists());
    }
}
