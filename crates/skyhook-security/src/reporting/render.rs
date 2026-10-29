//! The Markdown of `reports/`: each root's README and reproduction, each test's page and the
//! index. Every page opens with flat front matter its body repeats, uses fixed headings,
//! links only to files by relative path within its root's folder (or between a root's README
//! and the index), and sets authored text below its own headings with raw HTML neutralised.

use std::fmt::Write as _;

use super::{
    Document, IndexEntry, PocFolder,
    prose::{block as authored, line},
};
use crate::store::{
    ConfidenceTier, Gate, PocAttempt, ReportScenario, ReproductionOutcome, Resolution,
};

/// Flat YAML front matter of scalar `fields`, never read back by tooling.
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

/// `text` as an inline code span, fenced by more backticks than any run inside it.
fn code(text: &str) -> String {
    let longest = (text.split(|c| c != '`').map(str::len).max()).unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// The headline's empirical standing, with what a test that did not demonstrate it recorded.
fn testing(document: &Document) -> String {
    match &document.snapshot.content.headline.poc {
        Some(poc) if document.headline.confidence == ConfidenceTier::Demonstrated => {
            format!("Confirmed by review; demonstrated by PoC {}.", poc.id)
        }
        Some(poc) => format!(
            "Confirmed by review; not demonstrated: PoC {} {}.",
            poc.id, poc.result
        ),
        None => "Confirmed by review; empirically untested.".to_owned(),
    }
}

/// A scenario's conditions, each with what it gates and how it was resolved.
fn premises(scenario: &ReportScenario) -> String {
    let mut text = String::new();
    for condition in &scenario.candidate.premises {
        let premise = &condition.premise;
        let gates = match condition.gates {
            Gate::Metric { metric } => format!(" (gates {metric})"),
            Gate::Scenario => String::new(),
        };
        let _ = write!(
            text,
            "- Premise {}{gates}: {}",
            premise.id,
            line(&premise.statement)
        );
        match (premise.resolution.as_ref()).filter(|resolution| resolution.current) {
            Some(resolution) => {
                let _ = write!(text, " ({}", resolution.resolution.kind());
                if let Resolution::Holds {
                    deployment: Some(deployment),
                } = resolution.resolution
                {
                    let _ = write!(text, ", deployment fact {deployment} of the scope");
                }
                let _ = writeln!(text, "): {}", line(&resolution.note));
            }
            None => text.push_str(" (open)\n"),
        }
    }
    text
}

/// The headline scenario as the store holds it: the root flaw, the paths and sources it
/// starts from, each member flaw with its classification, possible effects and each path's
/// route to it, then the premises the assessment rests on.
fn scenario(document: &Document) -> String {
    let content = &document.snapshot.content;
    let candidate = &content.headline.candidate;
    let mut text = format!(
        "## Scenario\n\nRoot flaw {}: {}. Candidate {}: {}.\n\n{}",
        content.root.flaw.id,
        line(&content.root.flaw.title),
        candidate.candidate.id,
        line(&candidate.candidate.title),
        note(document, &content.headline)
    );
    for path in &content.paths {
        let sources: Vec<String> = path.sources.iter().map(ToString::to_string).collect();
        let _ = writeln!(
            text,
            "- Path {}: {} (sources {})",
            path.id,
            line(&path.description),
            sources.join(", ")
        );
    }
    for source in &content.sources {
        let _ = writeln!(
            text,
            "- Source {} ({}, {}): {}; entry {}",
            source.id,
            source.kind,
            source.trust,
            line(&source.description),
            code(&source.entry.to_string())
        );
    }
    if let Some(cwe) = candidate.cwe {
        let _ = writeln!(text, "- CWE-{cwe}");
    }
    text.push('\n');
    for (member, flaw) in candidate.members.iter().zip(&content.flaws) {
        let summary = &flaw.flaw;
        match member.step {
            Some(step) => {
                let _ = write!(text, "### Step {step}\n\nFlaw {}", summary.id);
            }
            None => {
                let _ = write!(text, "### Flaw {}\n\nFlaw {}", summary.id, summary.id);
            }
        }
        let _ = write!(text, ": {}. {}", line(&summary.title), summary.operation);
        if let Some(deficiency) = flaw.deficiency {
            let _ = write!(text, " · {} · {}", deficiency.guard, deficiency.deficiency);
        }
        let _ = write!(text, "\n\n{}\n", authored(&flaw.statement, 3));
        for effect in &flaw.effects {
            let _ = writeln!(
                text,
                "- {}: {}",
                effect.property,
                line(effect.note.as_str())
            );
        }
        text.push('\n');
        for exposure in (candidate.exposures.iter()).filter(|exposure| exposure.flaw == summary.id)
        {
            let _ = write!(
                text,
                "Along path {} ({}): {}\n\n",
                exposure.path,
                exposure.outcome,
                line(&exposure.conditions)
            );
            for (index, step) in exposure.route.iter().enumerate() {
                let _ = writeln!(
                    text,
                    "{}. {} ({}): {}",
                    index + 1,
                    code(&step.location.to_string()),
                    step.basis,
                    line(step.note.as_str())
                );
            }
            text.push('\n');
        }
    }
    if !candidate.premises.is_empty() {
        text.push_str("## Premises\n\n");
        text.push_str(&premises(&content.headline));
        text.push('\n');
    }
    text
}

/// The reporter's note on a scenario, as its own paragraph.
fn note(document: &Document, scenario: &ReportScenario) -> String {
    (document.content.notes.iter())
        .find(|note| note.candidate == scenario.candidate.candidate.id)
        .map_or_else(String::new, |note| {
            format!("{}\n", authored(note.note.as_str(), 3))
        })
}

/// The reporter's account of what a test showed, as its own paragraph.
fn narrative(document: &Document, poc: &PocAttempt, level: usize) -> String {
    (document.content.narratives.iter())
        .find(|narrative| narrative.poc == poc.id)
        .map_or_else(String::new, |narrative| {
            format!("{}\n", authored(narrative.narrative.as_str(), level))
        })
}

/// The root's other scenarios, each with its severity, standing, test and own conditions.
fn variants(document: &Document) -> String {
    let variants = &document.snapshot.content.variants;
    if variants.is_empty() {
        return String::new();
    }
    let mut text = String::from("## Variants\n\nOther scenarios this root flaw leads to:\n\n");
    for variant in variants {
        let candidate = &variant.candidate;
        let _ = write!(
            text,
            "### Candidate {}\n\nTitle: {}. {} · {} ({:.1}) · {} · {}\n\n{}\n{}",
            candidate.candidate.id,
            line(&candidate.candidate.title),
            variant.assessment.severity,
            code(&variant.assessment.vector.to_string()),
            variant.assessment.score,
            candidate.candidate.status,
            candidate
                .candidate
                .confidence
                .map_or("", ConfidenceTier::as_str),
            authored(&candidate.description, 3),
            note(document, variant)
        );
        if let Some(poc) = &variant.poc {
            let _ = writeln!(
                text,
                "- Test: [PoC {}](poc-{}/poc.md), {}",
                poc.id, poc.id, poc.result
            );
        }
        for exposure in &candidate.exposures {
            let _ = writeln!(
                text,
                "- Along path {} (flaw {}): {}",
                exposure.path,
                exposure.flaw,
                line(&exposure.conditions)
            );
        }
        text.push_str(&premises(variant));
        text.push('\n');
    }
    text
}

/// Each stated test's page and artifacts, by role.
fn artifacts(document: &Document) -> String {
    if document.pocs.is_empty() {
        return String::new();
    }
    let mut text = String::from("## PoC artifacts\n\n");
    for manifest in &document.pocs {
        let _ = writeln!(
            text,
            "- [PoC {}]({}/poc.md), candidate {}: {} files",
            manifest.poc,
            manifest.folder,
            manifest.candidate,
            manifest.files.len()
        );
        for file in &manifest.files {
            let _ = writeln!(
                text,
                "  - {}: [{}]({}/{}), {} bytes",
                file.role, file.path, manifest.folder, file.path, file.bytes
            );
        }
    }
    text.push('\n');
    text
}

pub(super) fn readme(document: &Document) -> String {
    let content = &document.snapshot.content;
    let input = &document.content;
    let headline = &content.headline;
    let root = content.root.flaw.id;
    let assessed = document.headline;
    let mut text = front_matter(&[
        ("type", "finding".to_owned()),
        ("root", root.to_string()),
        ("status", "current".to_owned()),
        ("severity", document.severity.to_string()),
        ("headline", assessed.candidate.to_string()),
        ("headline_severity", assessed.severity.to_string()),
        ("headline_score", format!("{:.1}", assessed.score)),
        ("headline_vector", assessed.vector.to_string()),
        ("headline_confidence", assessed.confidence.to_string()),
    ]);
    let _ = write!(
        text,
        "# Root flaw {root}\n\nTitle: {}\n\nStatus: current. Severity: {}, the most severe of its \
         scenarios. Headline: candidate {}, {}, score {:.1}, confidence {}. {} See the \
         [index](../index.md).\n\n\
         ## Summary\n\n{}\n## Analysis\n\n{}\n## Impact\n\n{}\n{}\
         ## CVSS 4.0\n\nThe headline scenario, scored for the impact if every premise holds.\n\n\
         - Vector: {}\n- CVSS-B score: {:.1} ({})\n- Confidence: {}\n\n{}\n\
         ## Analytical validation evidence\n\n{}\n",
        line(input.title.as_str()),
        document.severity,
        assessed.candidate,
        assessed.severity,
        assessed.score,
        assessed.confidence,
        testing(document),
        authored(input.summary.as_str(), 2),
        authored(input.analysis.as_str(), 2),
        authored(input.impact.as_str(), 2),
        scenario(document),
        code(&assessed.vector.to_string()),
        assessed.score,
        assessed.severity,
        assessed.confidence,
        authored(&headline.assessment.rationale, 2),
        authored(&headline.validation.evidence, 2),
    );
    if let Some(poc) = &headline.poc {
        let _ = write!(
            text,
            "## Empirical record\n\n[PoC {}](poc-{}/poc.md): {}.\n\n{}\n{}",
            poc.id,
            poc.id,
            poc.result,
            authored(&poc.evidence, 2),
            narrative(document, poc, 2)
        );
    }
    text.push_str(&variants(document));
    let _ = write!(
        text,
        "## Remediation\n\n{}\n",
        authored(input.remediation.as_str(), 2),
    );
    for section in &input.sections {
        let _ = write!(
            text,
            "## {}\n\n{}\n",
            line(section.heading.as_str()),
            authored(section.body.as_str(), 2)
        );
    }
    text.push_str(
        "## Reproduction\n\nSee [repro.md](repro.md) for reporter-authored instructions.\n\n",
    );
    text.push_str(&artifacts(document));
    text
}

pub(super) fn reproduction(document: &Document) -> String {
    let reproduction = &document.content.reproduction;
    let root = document.snapshot.content.root.flaw.id;
    let mut text = front_matter(&[
        ("type", "reproduction".to_owned()),
        ("root", root.to_string()),
    ]);
    let _ = write!(
        text,
        "# Reproduction\n\nRoot flaw {root}. {} These instructions are supplied by the reporter; \
         report generation executes nothing. See the [report](README.md).\n\n## Prerequisites\n\n",
        testing(document),
    );
    if reproduction.prerequisites.is_empty() {
        text.push_str("No additional prerequisites were supplied.\n\n");
    } else {
        for prerequisite in &reproduction.prerequisites {
            let _ = writeln!(text, "{}", authored(prerequisite.as_str(), 2));
        }
    }
    text.push_str("## Steps\n\n");
    for (index, step) in reproduction.steps.iter().enumerate() {
        let _ = write!(
            text,
            "### Step {}\n\n{}\n",
            index + 1,
            authored(step.instruction.as_str(), 3),
        );
        for cited in &step.artifacts {
            let file = (document.pocs.iter())
                .filter(|folder| folder.poc == cited.poc)
                .flat_map(|folder| folder.files.iter().map(move |file| (folder, file)))
                .find(|(_, file)| file.reference == cited.reference);
            if let Some((folder, file)) = file {
                let _ = writeln!(
                    text,
                    "- Uses [{}]({}/{}) from PoC {}",
                    file.path, folder.folder, file.path, folder.poc
                );
            }
        }
        if !step.artifacts.is_empty() {
            text.push('\n');
        }
        let _ = write!(
            text,
            "Expected:\n\n{}\n",
            authored(step.expected_result.as_str(), 3),
        );
    }
    match &reproduction.outcome {
        ReproductionOutcome::Unobserved {
            expected_result,
            limitations,
        } => {
            let _ = write!(
                text,
                "## Expected outcome (not observed)\n\n{}\n## Limitations\n\n{}\n",
                authored(expected_result.as_str(), 2),
                authored(limitations.as_str(), 2)
            );
        }
        ReproductionOutcome::Observed { observed_result } => {
            let _ = write!(
                text,
                "## Observed outcome\n\n{}\n",
                authored(observed_result.as_str(), 2)
            );
        }
    }
    text.push_str("## Cleanup instructions\n\n");
    if reproduction.cleanup.is_empty() {
        text.push_str("No cleanup instructions were supplied. No cleanup was performed.\n\n");
    } else {
        for instruction in &reproduction.cleanup {
            let _ = writeln!(text, "{}", authored(instruction.as_str(), 2));
        }
    }
    text.push_str(&artifacts(document));
    text
}

/// Whether an artifact is an image a viewer can show inline.
fn image(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

pub(super) fn poc(document: &Document, manifest: &PocFolder, poc: &PocAttempt) -> String {
    let root = document.snapshot.content.root.flaw.id;
    let mut text = front_matter(&[
        ("type", "poc".to_owned()),
        ("root", root.to_string()),
        ("poc", poc.id.to_string()),
        ("result", poc.result.to_string()),
    ]);
    let _ = write!(
        text,
        "# PoC {}\n\nRoot flaw {root}. A test of candidate {} against validation {}: {}. See the \
         [report](../README.md). Checksums are in [SHA256SUMS](SHA256SUMS) and the record in \
         [poc.json](poc.json).\n\n## Evidence\n\n{}\n{}## Artifacts\n\n",
        poc.id,
        poc.candidate,
        poc.validation,
        poc.result,
        authored(&poc.evidence, 2),
        narrative(document, poc, 2)
    );
    let mut role = None;
    for file in &manifest.files {
        if role != Some(file.role) {
            if role.is_some() {
                text.push('\n');
            }
            let _ = write!(text, "### {}\n\n", file.role);
            role = Some(file.role);
        }
        let _ = writeln!(
            text,
            "- [{}]({}), {} bytes, SHA-256 {}, from {}",
            file.path,
            file.path,
            file.bytes,
            code(&file.sha256.to_string()),
            code(file.reference.as_str())
        );
        if image(&file.path) {
            let _ = write!(text, "\n  ![{}]({})\n", file.path, file.path);
        }
    }
    text.push('\n');
    text
}

pub(super) fn index(entries: &[IndexEntry]) -> String {
    let mut text = front_matter(&[
        ("type", "index".to_owned()),
        ("reports", entries.len().to_string()),
    ]);
    let _ = write!(
        text,
        "# Reports\n\n{} current reports, one per root flaw. Machine-readable: \
         [index.json](index.json).\n\n",
        entries.len()
    );
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
                format!("[{}]({})", line(entry.title.as_str()), entry.report),
                entry.severity.to_string(),
                format!("candidate {}", headline.candidate),
                headline.severity.to_string(),
                format!("{:.1}", headline.score),
                headline.confidence.to_string(),
            ]
        })
        .collect();
    let table = skyhook_cli::Table {
        header,
        numeric_from: header.len(),
        rows,
    };
    table.markdown(&mut text);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_spans_outrun_the_backticks_they_hold() {
        assert_eq!(code("a `b`"), "`` a `b` ``");
    }
}
