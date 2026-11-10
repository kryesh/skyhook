//! The Markdown of `reports/`: each root's README, each test's page, the hardening list and
//! the index. Every page opens with flat front matter its body repeats, uses fixed headings,
//! links only to files by relative path within its root's folder (or between the index and a
//! root's README or the hardening list), and sets authored text below its own headings with
//! raw HTML neutralised.

use std::fmt::Write as _;

use super::{
    Document, IndexEntry, TestFolder,
    prose::{block as authored, flat, label, line},
};
use crate::store::{ArtifactRole, Hardening, HardeningReason, ReportedTest, TestResult};

/// Flat YAML front matter of scalar `fields`.
fn front_matter(fields: &[(&str, String)]) -> String {
    let mut text = String::from("---\n");
    for (name, value) in fields {
        let _ = writeln!(text, "{name}: {}", scalar(value));
    }
    text.push_str("---\n\n");
    text
}

/// A YAML scalar: plain when it is safe as one, otherwise double-quoted.
fn scalar(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if plain {
        value.to_owned()
    } else {
        format!("{value:?}")
    }
}

/// `text` as an inline code span on one line, fenced by more backticks than any run inside it.
fn code(text: &str) -> String {
    let text = &flat(text);
    let longest = (text.split(|c| c != '`').map(str::len).max()).unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// The roles a confirmed test records no artifact of, of the two a confirmation shows.
fn missing(test: &ReportedTest) -> Vec<ArtifactRole> {
    match test.result {
        TestResult::Confirmed => [ArtifactRole::Attack, ArtifactRole::Control]
            .into_iter()
            .filter(|role| !test.artifacts.iter().any(|artifact| artifact.role == *role))
            .collect(),
        _ => Vec::new(),
    }
}

/// A test's result, with the artifacts a confirmation lacks.
fn result(test: &ReportedTest) -> String {
    let missing = missing(test);
    match missing.is_empty() {
        true => test.result.to_string(),
        false => {
            let roles: Vec<_> = missing.iter().map(|role| role.as_str()).collect();
            format!("{}, no {} artifact", test.result, roles.join(" or "))
        }
    }
}

pub(super) fn readme(document: &Document) -> String {
    let headline = document.headline;
    let mut fields = vec![
        ("type", "report".to_owned()),
        ("root", document.root.to_string()),
        ("finding", document.finding.to_string()),
        ("severity", document.severity.to_string()),
        ("headline", headline.candidate.to_string()),
        ("confidence", headline.confidence.to_string()),
    ];
    if let Some(cwe) = document.cwe {
        fields.push(("cwe", format!("CWE-{cwe}")));
    }
    let mut text = front_matter(&fields);
    let _ = write!(
        text,
        "# {}\n\nRoot flaw {}, severity {}. Headline: candidate {}, {} ({:.1}), {}. \
         Machine-readable: [finding.json](finding.json); all reports: \
         [index](../index.md).\n\n## Scenarios\n\n",
        line(document.title),
        document.root,
        document.severity,
        headline.candidate,
        headline.severity,
        headline.score,
        headline.confidence,
    );
    let header = &[
        "Candidate",
        "Paths",
        "Chain",
        "Vector",
        "Score",
        "Severity",
        "Status",
        "Confidence",
        "Test",
    ];
    let rows = (document.scenarios.iter())
        .map(|scenario| {
            let chain: Vec<_> = scenario.chain.iter().map(ToString::to_string).collect();
            let actions: Vec<_> = (scenario.actions.iter())
                .map(|action| format!("{} {}", action.path, line(&action.title)))
                .collect();
            let test = match &scenario.test {
                Some(test) => format!(
                    "[test {}](test-{}/test.md) {}",
                    test.id,
                    test.id,
                    result(test)
                ),
                None => "untested".to_owned(),
            };
            vec![
                format!("{} {}", scenario.candidate, line(&scenario.title)),
                actions.join(" → "),
                chain.join(" → "),
                code(&scenario.vector.to_string()),
                format!("{:.1}", scenario.vector.score()),
                scenario.severity.to_string(),
                scenario.state.to_string(),
                scenario.confidence.to_string(),
                test,
            ]
        })
        .collect();
    skyhook::product::text::Table {
        header,
        numeric_from: header.len(),
        rows,
    }
    .markdown(&mut text);
    text.push('\n');
    text.push_str(&authored(document.body, 1));
    text
}

/// Whether an artifact is an image a viewer can show inline.
fn image(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

pub(super) fn test(document: &Document, folder: &TestFolder) -> String {
    let test = folder.test;
    let mut text = front_matter(&[
        ("type", "test".to_owned()),
        ("root", document.root.to_string()),
        ("test", test.id.to_string()),
        ("candidate", folder.candidate.to_string()),
        ("result", test.result.to_string()),
    ]);
    let _ = write!(
        text,
        "# Test {}\n\nRoot flaw {}. A test of candidate {}: {}. See the [report](../README.md). \
         Checksums are in [SHA256SUMS](SHA256SUMS) and the record in [test.json](test.json).\n\n",
        test.id,
        document.root,
        folder.candidate,
        result(test),
    );
    if let Some(note) = &test.note {
        let _ = write!(text, "## Account\n\n{}\n", authored(note, 2));
    }
    if let Some(environment) = &test.environment {
        let _ = write!(text, "## Environment\n\n{}\n", authored(environment, 2));
    }
    text.push_str("## Artifacts\n\n");
    let mut role = None;
    for file in &folder.files {
        let artifact = file.artifact;
        if role != Some(artifact.role) {
            if role.is_some() {
                text.push('\n');
            }
            let _ = write!(text, "### {}\n\n", artifact.role);
            role = Some(artifact.role);
        }
        let _ = writeln!(
            text,
            "- [{}]({}), SHA-256 {}, from {}",
            file.path,
            file.path,
            code(&skyhook_media::BlobDigest::from_bytes(artifact.digest).to_string()),
            code(artifact.reference.as_str())
        );
        if image(&file.path) {
            let _ = write!(text, "\n  ![{}]({})\n", file.path, file.path);
        }
    }
    text.push('\n');
    text
}

pub(super) fn index(entries: &[IndexEntry], hardening: usize) -> String {
    let mut text = front_matter(&[
        ("type", "index".to_owned()),
        ("reports", entries.len().to_string()),
        ("hardening", hardening.to_string()),
    ]);
    let _ = write!(
        text,
        "# Reports\n\n{} current reports, one per root flaw. Machine-readable: \
         [index.json](index.json).\n\n",
        entries.len()
    );
    if hardening > 0 {
        let _ = write!(
            text,
            "[Hardening](hardening.md): {hardening} confirmed flaws no report covers, with no \
             chain found or chains validated below the reporting threshold.\n\n"
        );
    }
    if entries.is_empty() {
        return text;
    }
    let header = &[
        "Root flaw",
        "Report",
        "Severity",
        "Headline",
        "Headline severity",
        "Headline score",
        "Confidence",
    ];
    let rows = (entries.iter())
        .map(|entry| {
            let headline = entry.headline;
            vec![
                entry.root.to_string(),
                format!("[{}]({})", label(entry.title), entry.report),
                entry.severity.to_string(),
                format!("candidate {}", headline.candidate),
                headline.severity.to_string(),
                format!("{:.1}", headline.score),
                headline.confidence.to_string(),
            ]
        })
        .collect();
    let table = skyhook::product::text::Table {
        header,
        numeric_from: header.len(),
        rows,
    };
    table.markdown(&mut text);
    text
}

pub(super) fn hardening(flaws: &[Hardening]) -> String {
    let mut text = front_matter(&[
        ("type", "hardening".to_owned()),
        ("flaws", flaws.len().to_string()),
    ]);
    let _ = write!(
        text,
        "# Hardening\n\n{} confirmed flaws no report covers: verified defects that are not \
         reported. All reports: [index](index.md).\n",
        flaws.len()
    );
    let (unchained, below): (Vec<_>, Vec<_>) =
        (flaws.iter()).partition(|flaw| matches!(flaw.reason, HardeningReason::NoChain { .. }));
    let group = |text: &mut String, heading, about, header: &[&str], flaws: &[&Hardening]| {
        if flaws.is_empty() {
            return;
        }
        let _ = write!(text, "\n## {heading}\n\n{} {about}\n\n", flaws.len());
        let rows = (flaws.iter())
            .map(|flaw| {
                let mut row = vec![
                    flaw.flaw.to_string(),
                    line(&flaw.title),
                    code(&flaw.location.to_string()),
                    flaw.cwe
                        .map_or_else(String::new, |cwe| format!("CWE-{cwe}")),
                ];
                match &flaw.reason {
                    HardeningReason::NoChain { note } => row.push(line(note)),
                    HardeningReason::BelowThreshold {
                        candidate,
                        severity,
                    } => row.extend([candidate.to_string(), severity.to_string()]),
                }
                row
            })
            .collect();
        skyhook::product::text::Table {
            header,
            numeric_from: header.len(),
            rows,
        }
        .markdown(text);
    };
    group(
        &mut text,
        "No chain found",
        "confirmed flaws no validated candidate holds: no chain to an observable consequence \
         was found, so they are not scored or tested.",
        &["Flaw", "Title", "Location", "CWE", "Why no chain"],
        &unchained,
    );
    group(
        &mut text,
        "Below the reporting threshold",
        "confirmed flaws whose most severe validated candidate is assessed below the run's \
         threshold.",
        &["Flaw", "Title", "Location", "CWE", "Candidate", "Severity"],
        &below,
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_spans_outrun_the_backticks_they_hold() {
        assert_eq!(code("a `b`"), "`` a `b` ``");
    }

    /// A location written over several lines keeps its hardening row on one line.
    #[test]
    fn a_multiline_location_stays_in_its_row() {
        use crate::store::{CodeSite, FlawId, Location, tests::text};
        let flaw = Hardening {
            flaw: FlawId::try_from(7).unwrap(),
            title: "copy".to_owned(),
            location: Location::Code(CodeSite {
                file: text("src/a.rs"),
                start_line: 1,
                end_line: 2,
                function: Some(text("parse\r\n\nheader")),
            }),
            cwe: None,
            reason: HardeningReason::NoChain {
                note: "no path".to_owned(),
            },
        };
        let page = hardening(&[flaw]);
        let row = "| 7 | copy | `src/a.rs:1-2 in parse header` |  | no path |\n";
        assert!(page.contains(row), "{page}");
    }
}
