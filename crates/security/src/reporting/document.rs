//! What a report's folder renders: its `finding.json` document, the index entries, and the
//! layout of files and folders it holds.

use super::*;

/// A report's `finding.json`: what its pages render.
#[derive(Serialize)]
pub(super) struct Document<'a> {
    pub(super) format_version: u32,
    pub(super) run: skyhook::identity::SessionId,
    pub(super) finding: FindingId,
    pub(super) root: FlawId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cwe: Option<u32>,
    pub(super) title: &'a str,
    /// The highest among its scenarios.
    pub(super) severity: Severity,
    pub(super) headline: Headline,
    pub(super) scenarios: &'a [ReportedScenario],
    pub(super) body: &'a str,
    /// A folder per stated test, the headline's first.
    pub(super) tests: Vec<TestFolder<'a>>,
}

/// The headline scenario: the most severe demonstrated one, otherwise the most severe.
#[derive(Clone, Copy, Serialize)]
pub(super) struct Headline {
    pub(super) candidate: CandidateId,
    pub(super) severity: Severity,
    pub(super) score: f64,
    pub(super) vector: Cvss40,
    pub(super) confidence: ConfidenceTier,
}

/// One stated test's folder beside the report.
#[derive(Serialize)]
pub(super) struct TestFolder<'a> {
    pub(super) test: &'a ReportedTest,
    pub(super) candidate: CandidateId,
    /// Relative to the report's folder.
    pub(super) folder: String,
    pub(super) files: Vec<ArtifactCopy<'a>>,
}

/// An artifact as its test's folder holds it: `role/name`, numbered on a clash.
#[derive(Serialize)]
pub(super) struct ArtifactCopy<'a> {
    pub(super) path: String,
    #[serde(flatten)]
    pub(super) artifact: &'a RecordedArtifact,
}

/// What `reports/index.json` lists for one root's current report.
#[derive(Serialize)]
pub(super) struct IndexEntry<'a> {
    pub(super) root: FlawId,
    pub(super) finding: FindingId,
    pub(super) title: &'a str,
    /// The highest among the finding's scenarios.
    pub(super) severity: Severity,
    pub(super) headline: Headline,
    /// Relative to `reports/`.
    pub(super) report: String,
}

#[derive(Serialize)]
pub(super) struct Index<'a> {
    pub(super) format_version: u32,
    pub(super) reports: &'a [IndexEntry<'a>],
    pub(super) hardening: &'a [Hardening],
}

pub(super) fn document<'a>(
    run: skyhook::identity::SessionId,
    report: &'a CurrentReport,
) -> Document<'a> {
    let scenarios = &report.scenarios;
    // The store lists them most severe first.
    let headline = (scenarios.iter())
        .find(|scenario| scenario.confidence == ConfidenceTier::Demonstrated)
        .unwrap_or(&scenarios[0]);
    let mut stated: Vec<&ReportedScenario> = vec![headline];
    stated.extend(
        scenarios
            .iter()
            .filter(|scenario| scenario.candidate != headline.candidate),
    );
    Document {
        format_version: 8,
        run,
        finding: report.finding,
        root: report.root,
        cwe: report.cwe,
        title: &report.title,
        severity: scenarios[0].severity,
        headline: Headline {
            candidate: headline.candidate,
            severity: headline.severity,
            score: headline.vector.score(),
            vector: headline.vector,
            confidence: headline.confidence,
        },
        scenarios,
        body: &report.body,
        tests: (stated.into_iter())
            .filter_map(|scenario| {
                let test = scenario.test.as_ref()?;
                let mut taken = BTreeSet::new();
                Some(TestFolder {
                    test,
                    candidate: scenario.candidate,
                    folder: format!("test-{}", test.id),
                    files: (test.artifacts.iter())
                        .map(|artifact| ArtifactCopy {
                            path: file_path(&mut taken, &artifact.reference, artifact.role),
                            artifact,
                        })
                        .collect(),
                })
            })
            .collect(),
    }
}

/// A file of a report's folder: rendered text, or an artifact copied from the run.
pub(super) enum Content<'a> {
    Text(Vec<u8>),
    Artifact(&'a RecordedArtifact),
}

/// What a report's folder holds: each file by its path below it, and each folder below it.
pub(super) struct Layout<'a> {
    pub(super) files: Vec<(String, Content<'a>)>,
    pub(super) folders: BTreeSet<String>,
}

pub(super) fn layout<'a>(document: &'a Document<'a>) -> Result<Layout<'a>> {
    let mut json = serde_json::to_vec_pretty(document)?;
    json.push(b'\n');
    let text = |path: &str, bytes: Vec<u8>| (path.to_owned(), Content::Text(bytes));
    let mut layout = Layout {
        files: vec![
            text("README.md", render::readme(document).into_bytes()),
            text("finding.json", json),
        ],
        folders: BTreeSet::new(),
    };
    for folder in &document.tests {
        let mut json = serde_json::to_vec_pretty(folder)?;
        json.push(b'\n');
        let sums: String = (folder.files.iter())
            .map(|file| {
                format!(
                    "{}  {}\n",
                    BlobDigest::from_bytes(file.artifact.digest),
                    file.path
                )
            })
            .collect();
        layout.folders.insert(folder.folder.clone());
        for file in &folder.files {
            (layout.folders).insert(format!("{}/{}", folder.folder, file.artifact.role));
            (layout.files).push((
                format!("{}/{}", folder.folder, file.path),
                Content::Artifact(file.artifact),
            ));
        }
        let page = render::test(document, folder).into_bytes();
        for (name, bytes) in [
            ("test.json", json),
            ("test.md", page),
            ("SHA256SUMS", sums.into_bytes()),
        ] {
            (layout.files).push(text(&format!("{}/{name}", folder.folder), bytes));
        }
    }
    Ok(layout)
}

/// `role/name` for an artifact: its reference's own name, which the reference already keeps
/// to portable characters, with `-2`, `-3`… before the extension when another artifact of the
/// test takes it in any letter case.
pub(super) fn file_path(
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
