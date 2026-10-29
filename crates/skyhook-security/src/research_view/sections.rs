//! The Research tab's record sections: what the run holds of each kind, the first of them
//! once a section is open, and the full record once the viewer opens it.
use std::fmt::Display;

use skyhook_cli::page::{
    Block, Cell, Column, Detail, Disclosed, Record, RecordKey, Section, SectionKey, Tone,
};

use super::words;
use crate::store::{
    About, CandidateDetails, CandidateId, Check, ComponentCoverage, ComponentId, ConfidenceTier,
    Coverage, Exposure, FindingStatus, FlawId, OpenTest, Outcome, PathCoverage, PathId, PlanUnit,
    PremiseId, PremiseOn, Resolution, ResolutionKind, ReviewState, RouteStep, RunState, Status,
    Store, UnitWork, Verdict,
};

/// The kinds of record the tab lists, in reading order.
#[derive(Clone, Copy)]
enum Shelf {
    Candidates,
    Findings,
    Paths,
    Plan,
    Flaws,
    Components,
    Sources,
    Premises,
}

impl Shelf {
    fn key(self) -> SectionKey {
        SectionKey(match self {
            Self::Candidates => "candidates",
            Self::Findings => "findings",
            Self::Paths => "paths",
            Self::Plan => "plan",
            Self::Flaws => "flaws",
            Self::Components => "components",
            Self::Sources => "sources",
            Self::Premises => "premises",
        })
    }
}

type Result<T> = std::result::Result<T, crate::store::Error>;

/// Every section in reading order, with records and detail for what `open` shows; a
/// candidate with an open test shows it as proving, or abandoned once its job ended.
pub fn sections(
    store: &Store,
    state: &RunState,
    coverage: &Coverage,
    open: &Disclosed,
    tests: &[OpenTest],
) -> Result<Vec<Block>> {
    let progress = &state.progress;
    let candidates: u64 = progress.candidates.values().sum();
    let flaws = progress.flaws + state.refuted;
    let shelf =
        |shelf: Shelf, title: &str, total, summary: Option<String>, columns: &[(&str, u8)]| {
            (
                shelf,
                Section {
                    key: shelf.key(),
                    title: title.to_owned(),
                    summary,
                    total,
                    columns: (columns.iter())
                        .map(|(title, priority)| Column {
                            title: (*title).to_owned(),
                            priority: *priority,
                        })
                        .collect(),
                    records: Vec::new(),
                },
            )
        };
    let swept = |outcome| progress.sweeps.get(&outcome).copied().unwrap_or_default();
    let mut sections = vec![
        shelf(
            Shelf::Candidates,
            "Candidates",
            candidates,
            Some(format!(
                "{} shortlisted · {} confirmed · {} demonstrated · {} proving",
                progress.candidates_in(Status::Shortlisted),
                progress.candidates_at(ConfidenceTier::Confirmed),
                progress.candidates_at(ConfidenceTier::Demonstrated),
                tests.iter().filter(|test| test.live).count()
            )),
            &[
                ("id", 0),
                ("status", 0),
                ("severity", 1),
                ("confidence", 3),
                ("members", 4),
                ("paths", 4),
                ("title", 0),
            ],
        ),
        shelf(
            Shelf::Findings,
            "Findings",
            state.recorded_findings,
            None,
            &[
                ("root flaw", 0),
                ("status", 1),
                ("test", 2),
                ("published", 3),
                ("title", 0),
            ],
        ),
        shelf(
            Shelf::Paths,
            "Paths",
            progress.paths,
            Some(format!(
                "{} checked · {} unchecked · {} unreached",
                coverage.paths_in(PathCoverage::Checked),
                coverage.paths_in(PathCoverage::Unchecked),
                coverage.paths_in(PathCoverage::Unreached)
            )),
            &[
                ("id", 0),
                ("coverage", 0),
                ("review", 1),
                ("sources", 2),
                ("components", 3),
                ("description", 0),
            ],
        ),
        shelf(
            Shelf::Plan,
            "Plan",
            state.facts.synthesis.total(),
            Some(state.facts.synthesis.line()),
            &[
                ("id", 0),
                ("kind", 0),
                ("subject", 2),
                ("state", 0),
                ("limitation", 1),
            ],
        ),
        shelf(
            Shelf::Flaws,
            "Flaws",
            flaws,
            Some(format!(
                "{} open · {} refuted",
                progress.flaws, state.refuted
            )),
            &[
                ("id", 0),
                ("component", 2),
                ("operation", 1),
                ("deficiency", 3),
                ("effects", 2),
                ("owed", 1),
                ("state", 0),
                ("title", 0),
            ],
        ),
        shelf(
            Shelf::Components,
            "Components",
            progress.components,
            Some(format!(
                "swept {} · blocked {}",
                swept(Outcome::Completed),
                swept(Outcome::Blocked)
            )),
            &[
                ("id", 0),
                ("sweep", 0),
                ("coverage", 2),
                ("files", 3),
                ("paths", 1),
                ("flaws", 1),
                ("name", 0),
            ],
        ),
        shelf(
            Shelf::Sources,
            "Sources",
            progress.sources,
            None,
            &[
                ("id", 0),
                ("kind", 1),
                ("trust", 1),
                ("entry", 2),
                ("paths", 3),
                ("description", 0),
            ],
        ),
        shelf(
            Shelf::Premises,
            "Premises",
            state.premises,
            Some(format!("{} open", coverage.open_premises)),
            &[("id", 0), ("owner", 1), ("state", 0), ("statement", 0)],
        ),
    ];
    for (shelf, section) in &mut sections {
        if let Some(limit) = open.limit(section.key) {
            section.records = records(store, *shelf, limit, open, tests)?;
        }
    }
    Ok(sections
        .into_iter()
        .map(|(_, section)| Block::Section(section))
        .collect())
}

fn records(
    store: &Store,
    shelf: Shelf,
    limit: usize,
    open: &Disclosed,
    tests: &[OpenTest],
) -> Result<Vec<Record>> {
    let key = shelf.key();
    let record = |id: &dyn Display, cells: Vec<Cell>, detail: &dyn Fn() -> Result<Vec<Detail>>| {
        let key_of = RecordKey(id.to_string());
        let detail = match open.is_open(key, &key_of) {
            true => Some(detail()?),
            false => None,
        };
        Ok(Record {
            key: key_of,
            cells,
            detail,
        })
    };
    match shelf {
        Shelf::Candidates => (store.candidate_rows(limit)?.into_iter())
            .map(|row| {
                let candidate = row.candidate;
                record(
                    &candidate.id,
                    vec![
                        id(candidate.id),
                        match tests.iter().find(|test| test.candidate == candidate.id) {
                            Some(test) => testing(candidate.status, test.live),
                            None => status(candidate.status),
                        },
                        optional(candidate.severity),
                        confidence(candidate.confidence),
                        row.members.to_string().into(),
                        row.paths.to_string().into(),
                        candidate.title.into(),
                    ],
                    &|| candidate_detail(store, candidate.id),
                )
            })
            .collect(),
        Shelf::Findings => (store.finding_rows(limit)?.into_iter())
            .map(|row| {
                let finding = &row.finding;
                let test = row
                    .poc
                    .map_or_else(|| "untested".into(), |result| words(result).into());
                let report = match finding.status {
                    FindingStatus::Current => format!("reports/{}/", finding.root),
                    _ => "not in reports/".to_owned(),
                };
                record(
                    &finding.id,
                    vec![
                        id(finding.root),
                        finding_status(finding.status),
                        test,
                        Cell::new(finding.at.format("%Y-%m-%d %H:%M").to_string(), Tone::Muted),
                        row.title.into(),
                    ],
                    &|| Ok(vec![line("report", vec![report.clone().into()])]),
                )
            })
            .collect(),
        Shelf::Paths => (store.path_rows(limit)?.into_iter())
            .map(|row| {
                let coverage = Cell::new(
                    words(row.coverage),
                    match row.coverage {
                        PathCoverage::Checked => Tone::Success,
                        PathCoverage::Unchecked => Tone::Accent,
                        PathCoverage::Unreached => Tone::Warning,
                    },
                );
                record(
                    &row.id,
                    vec![
                        id(row.id),
                        coverage,
                        match row.review {
                            ReviewState::NotPlanned => dash(),
                            ReviewState::Open => unit_state(None),
                            ReviewState::Ended(outcome) => unit_state(Some(outcome)),
                        },
                        row.sources.to_string().into(),
                        row.components.to_string().into(),
                        row.description.into(),
                    ],
                    &|| path_detail(store, row.id),
                )
            })
            .collect(),
        Shelf::Plan => (store.plan_rows(limit)?.into_iter())
            .map(|unit| {
                let (kind, subject) = match &unit.work {
                    UnitWork::Sweep { component } => ("sweep", format!("component #{component}")),
                    UnitWork::PathReview { path } => ("path review", format!("path #{path}")),
                    UnitWork::Question {
                        check: Some(check), ..
                    } => ("check", contract(check)),
                    UnitWork::Question { question, .. } => ("question", question.to_string()),
                    UnitWork::FollowUp { question, .. } => ("follow up", question.to_string()),
                };
                let limitation = unit
                    .outcome
                    .as_ref()
                    .map_or_else(dash, |outcome| Cell::new(outcome.note.clone(), Tone::Muted));
                record(
                    &unit.id,
                    vec![
                        id(unit.id),
                        kind.into(),
                        subject.into(),
                        unit_state(unit.outcome.as_ref().map(|outcome| outcome.outcome)),
                        limitation,
                    ],
                    &|| Ok(unit_detail(&unit)),
                )
            })
            .collect(),
        Shelf::Flaws => (store.flaw_rows(limit)?.into_iter())
            .map(|row| {
                let effects: Vec<String> = (row.effects.iter())
                    .map(|property| property.to_string()[..1].to_uppercase())
                    .collect();
                let deficiency = row.deficiency.map_or_else(dash, |gap| {
                    format!("{} {}", words(gap.guard), words(gap.deficiency)).into()
                });
                let owed = Cell::new(
                    row.owed.to_string(),
                    if row.owed > 0 {
                        Tone::Warning
                    } else {
                        Tone::Muted
                    },
                );
                let state = match row.refuted {
                    true => Cell::new("refuted", Tone::Muted),
                    false => Cell::new("open", Tone::Accent),
                };
                record(
                    &row.id,
                    vec![
                        id(row.id),
                        Cell::new(row.component, Tone::Muted),
                        words(row.operation).into(),
                        deficiency,
                        effects.join(" ").into(),
                        owed,
                        state,
                        row.title.into(),
                    ],
                    &|| flaw_detail(store, row.id),
                )
            })
            .collect(),
        Shelf::Components => (store.component_rows(limit)?.into_iter())
            .map(|row| {
                let component = row.component;
                let sweep = match component.swept {
                    Some(Outcome::Completed) => Cell::new("swept", Tone::Success),
                    Some(outcome) => Cell::new(words(outcome), Tone::Warning),
                    None => Cell::new("unswept", Tone::Muted),
                };
                let coverage = Cell::new(
                    words(component.coverage),
                    match component.coverage {
                        ComponentCoverage::Inspected => Tone::Success,
                        ComponentCoverage::Reached => Tone::Normal,
                        ComponentCoverage::Mapped => Tone::Muted,
                    },
                );
                record(
                    &component.id,
                    vec![
                        id(component.id),
                        sweep,
                        coverage,
                        row.files.to_string().into(),
                        row.paths.to_string().into(),
                        row.flaws.to_string().into(),
                        component.name.into(),
                    ],
                    &|| component_detail(store, component.id),
                )
            })
            .collect(),
        Shelf::Sources => (store.source_rows(limit)?.into_iter())
            .map(|row| {
                let source = &row.source;
                record(
                    &source.id,
                    vec![
                        id(source.id),
                        words(source.kind).into(),
                        words(source.trust).into(),
                        Cell::new(source.entry.to_string(), Tone::Muted),
                        row.paths.to_string().into(),
                        source.description.clone().into(),
                    ],
                    &|| {
                        Ok(vec![
                            line("entry", vec![source.entry.to_string().into()]),
                            line("principals", vec![ids(source.principals.iter()).into()]),
                            line("description", vec![source.description.clone().into()]),
                        ])
                    },
                )
            })
            .collect(),
        Shelf::Premises => (store.premise_rows(limit)?.into_iter())
            .map(|row| {
                record(
                    &row.id,
                    vec![
                        id(row.id),
                        owner(&row.on).into(),
                        standing(row.standing),
                        row.statement.into(),
                    ],
                    &|| premise_detail(store, row.id),
                )
            })
            .collect(),
    }
}

fn candidate_detail(store: &Store, candidate: CandidateId) -> Result<Vec<Detail>> {
    let CandidateDetails {
        description,
        cwe,
        members,
        roots,
        paths,
        sources,
        premises,
        exposures,
        assessment,
        ..
    } = store.candidate(candidate)?;
    let mut lines = vec![line("scenario", vec![description.into()])];
    if let Some(cwe) = cwe {
        lines.push(line("cwe", vec![format!("CWE-{cwe}").into()]));
    }
    let sources: Vec<String> = (sources.iter())
        .map(|source| {
            let description = store.source(*source).map(|source| source.description);
            format!("#{} {}", source, description.unwrap_or_default())
        })
        .collect();
    lines.push(line("roots", vec![ids(roots.iter()).into()]));
    lines.push(line("paths", vec![ids(paths.iter()).into()]));
    lines.extend(listed(
        "sources",
        sources.into_iter().map(|source| vec![source.into()]),
    ));
    lines.extend(listed(
        "members",
        members.into_iter().map(|member| {
            let step = member
                .step
                .map_or_else(|| "·".to_owned(), |step| format!("{step}."));
            vec![step.into(), id(member.flaw.id), member.flaw.title.into()]
        }),
    ));
    lines.extend(routes(&exposures));
    lines.extend(listed(
        "premises",
        premises.into_iter().map(|condition| {
            vec![
                self::standing(condition.premise.standing()),
                Cell::new(owner(&condition.premise.on), Tone::Muted),
                condition.premise.statement.into(),
            ]
        }),
    ));
    lines.push(match assessment {
        Some(assessment) => line(
            "assessed",
            vec![
                assessment.vector.to_string().into(),
                Cell::new(format!("· {:.1} ·", assessment.score), Tone::Muted),
                Cell::new(words(assessment.severity), Tone::Accent),
                Cell::new(format!("· {}", words(assessment.confidence)), Tone::Muted),
            ],
        ),
        None => line("assessed", vec![Cell::new("not yet", Tone::Muted)]),
    });
    let latest = store.latest(candidate)?;
    let review = (latest.verdict.as_ref()).map_or_else(|| "—".to_owned(), verdict);
    let poc = latest.poc.map_or_else(|| "—".to_owned(), words);
    let report = latest.finding.map_or_else(
        || "—".to_owned(),
        |finding| format!("finding {} {}", finding.id, finding.status),
    );
    lines.push(line(
        "review",
        vec![
            review.into(),
            Cell::new("· poc", Tone::Muted),
            poc.into(),
            Cell::new("· report", Tone::Muted),
            report.into(),
        ],
    ));
    Ok(lines)
}

fn path_detail(store: &Store, path: PathId) -> Result<Vec<Detail>> {
    let record = store.path(path)?;
    let work = store.path_work(path)?;
    let exposures = store.path_exposures(path)?;
    let mut lines = vec![
        line("handling", vec![record.description.into()]),
        line("sources", vec![ids(record.sources.iter()).into()]),
    ];
    lines.extend(listed(
        "reaches",
        (record.reaches.into_iter()).map(|reach| vec![id(reach.component), reach.note.into()]),
    ));
    lines.push(line(
        "owed",
        vec![Cell::new(
            format!(
                "{} decisions · {} unplaced",
                work.decisions.len(),
                work.unplaced.len()
            ),
            if work.decisions.is_empty() {
                Tone::Muted
            } else {
                Tone::Warning
            },
        )],
    ));
    lines.extend(listed(
        "exposures",
        exposures.into_iter().map(|exposure| {
            vec![
                id(exposure.flaw),
                Cell::new(
                    noted(words(exposure.outcome), "stale", !exposure.current),
                    Tone::Accent,
                ),
                exposure.title.into(),
            ]
        }),
    ));
    lines.extend(listed(
        "deferred",
        record.deferred.into_iter().map(|deferred| {
            vec![noted(deferred.reason, "settled", deferred.settled.is_some()).into()]
        }),
    ));
    Ok(lines)
}

/// A unit's question and what settles it, and how it ended.
fn unit_detail(unit: &PlanUnit) -> Vec<Detail> {
    let mut lines = Vec::new();
    if let UnitWork::Question {
        question, stopping, ..
    }
    | UnitWork::FollowUp {
        question, stopping, ..
    } = &unit.work
    {
        lines.push(line("question", vec![question.to_string().into()]));
        lines.push(line("settled by", vec![stopping.to_string().into()]));
    }
    if let Some(check) = unit.work.check() {
        lines.push(line("contract", vec![contract(check).into()]));
    }
    for about in unit.work.about() {
        let (label, record) = match about {
            About::Flaw(id) => ("flaw", format!("#{id}")),
            About::Candidate(id) => ("candidate", format!("#{id}")),
            About::Guarantee(id) => ("guarantee", format!("#{id}")),
            About::Premise(id) => ("premise", format!("#{id}")),
            About::Function {
                component,
                function,
            } => ("function", format!("{function} in component #{component}")),
        };
        lines.push(line(label, vec![record.into()]));
    }
    lines.push(match &unit.outcome {
        Some(outcome) => line(
            "outcome",
            vec![
                unit_state(unit.outcome.as_ref().map(|outcome| outcome.outcome)),
                outcome.note.clone().into(),
                Cell::new(
                    format!(
                        "· {} {}",
                        outcome.actor,
                        outcome.at.format("%Y-%m-%d %H:%M")
                    ),
                    Tone::Muted,
                ),
            ],
        ),
        None => line("outcome", vec![Cell::new("open", Tone::Accent)]),
    });
    lines
}

fn flaw_detail(store: &Store, flaw: FlawId) -> Result<Vec<Detail>> {
    let details = store.flaw(flaw)?;
    let record = details.record;
    let mut lines = vec![line("statement", vec![record.statement.into()])];
    if let Some(gap) = record.deficiency {
        lines.push(line(
            "deficiency",
            vec![format!("{} {}", words(gap.guard), words(gap.deficiency)).into()],
        ));
    }
    lines.extend(listed(
        "effects",
        (record.effects.into_iter()).map(|effect| {
            vec![
                Cell::new(words(effect.property), Tone::Accent),
                effect.note.to_string().into(),
            ]
        }),
    ));
    lines.extend(listed(
        "evidence",
        record.evidence.into_iter().map(|evidence| {
            vec![
                Cell::new(evidence.location.to_string(), Tone::Muted),
                format!("{} {}", words(evidence.role), words(evidence.stance)).into(),
                evidence.note.into(),
            ]
        }),
    ));
    lines.extend(routes(&details.exposures));
    lines.extend(listed(
        "relations",
        details.relations.into_iter().map(|relation| {
            let other = if relation.a == flaw {
                relation.b
            } else {
                relation.a
            };
            vec![words(relation.kind).into(), id(other), relation.note.into()]
        }),
    ));
    if let Some(refutation) = details.refutation {
        lines.push(line(
            "refuted",
            vec![noted(refutation.reason, "lifted", refutation.lifted).into()],
        ));
    }
    Ok(lines)
}

fn component_detail(store: &Store, component: ComponentId) -> Result<Vec<Detail>> {
    let dossier = store.dossier(component)?;
    let mut lines = vec![
        line("description", vec![dossier.component.description.into()]),
        line(
            "files",
            vec![Cell::new(dossier.files.join(", "), Tone::Muted)],
        ),
        line("reached by", vec![ids(dossier.reached_by.iter()).into()]),
    ];
    if let Some(outcome) = dossier
        .sweeps
        .iter()
        .rev()
        .find_map(|unit| unit.outcome.as_ref())
    {
        lines.push(line(
            "sweep",
            vec![words(outcome.outcome).into(), outcome.note.clone().into()],
        ));
    }
    lines.extend(listed(
        "guarantees",
        (dossier.guarantees.into_iter()).map(|guarantee| {
            vec![
                Cell::new(words(guarantee.kind), Tone::Muted),
                guarantee.statement.into(),
                Cell::new(format!("if {}", guarantee.preconditions), Tone::Muted),
            ]
        }),
    ));
    lines.extend(listed(
        "checks",
        (dossier.checks.into_iter()).map(|contract| {
            vec![
                id(contract.id),
                contract.function.into(),
                Cell::new(words(contract.kind), Tone::Muted),
                words(contract.state).into(),
            ]
        }),
    ));
    lines.extend(listed(
        "deferred",
        dossier.deferred.into_iter().map(|deferred| {
            let at = deferred
                .location
                .map_or_else(|| "—".to_owned(), |location| location.to_string());
            vec![
                Cell::new(at, Tone::Muted),
                noted(deferred.reason, "settled", deferred.settled.is_some()).into(),
            ]
        }),
    ));
    lines.extend(listed(
        "flaws",
        (dossier.flaws.into_iter()).map(|flaw| vec![id(flaw.id), flaw.title.into()]),
    ));
    Ok(lines)
}

fn premise_detail(store: &Store, premise: PremiseId) -> Result<Vec<Detail>> {
    let premise = store.premise(premise)?;
    let mut lines = vec![
        line("statement", vec![premise.statement.into()]),
        line("kind", vec![words(premise.kind).into()]),
        line("owner", vec![owner(&premise.on).into()]),
    ];
    if let Some(resolution) = premise.resolution {
        let mut cells = vec![
            Cell::new(
                noted(
                    words(resolution.resolution.kind()),
                    "stale",
                    !resolution.current,
                ),
                Tone::Accent,
            ),
            resolution.note.into(),
        ];
        if let Resolution::Fails { flaw: Some(flaw) } = resolution.resolution {
            cells.push(id(flaw));
        }
        lines.push(line("resolution", cells));
    }
    if !premise.undermined_by.is_empty() {
        lines.push(line(
            "undermined by",
            vec![ids(premise.undermined_by.iter()).into()],
        ));
    }
    Ok(lines)
}

/// Each exposure's decision and its route, a step a line.
fn routes(exposures: &[Exposure]) -> Vec<Detail> {
    let mut lines = Vec::new();
    for exposure in exposures {
        let mut head = vec![
            Cell::new(
                format!("path #{} → flaw #{}:", exposure.path, exposure.flaw),
                Tone::Muted,
            ),
            Cell::new(
                noted(words(exposure.outcome), "stale", !exposure.current),
                Tone::Accent,
            ),
        ];
        if !exposure.conditions.is_empty() {
            head.push(exposure.conditions.clone().into());
        }
        lines.push(Detail {
            label: lines.is_empty().then(|| "routes".to_owned()),
            cells: head,
        });
        lines.extend(
            exposure
                .route
                .iter()
                .enumerate()
                .map(|(index, step)| step_line(index, step)),
        );
    }
    lines
}

fn step_line(index: usize, step: &RouteStep) -> Detail {
    Detail {
        label: None,
        cells: vec![
            Cell::new(format!("  {}.", index + 1), Tone::Muted),
            step.location.to_string().into(),
            Cell::new(words(step.basis), Tone::Muted),
            step.note.to_string().into(),
        ],
    }
}

fn line(label: &str, cells: Vec<Cell>) -> Detail {
    Detail {
        label: Some(label.to_owned()),
        cells,
    }
}

/// A check's contract: its function, component and kind.
fn contract(check: &Check) -> String {
    let kind = words(check.kind);
    format!(
        "{} in component #{} · {kind}",
        check.function, check.component
    )
}

/// One line per item, the first labelled.
fn listed(label: &str, items: impl Iterator<Item = Vec<Cell>>) -> Vec<Detail> {
    (items.enumerate())
        .map(|(index, cells)| Detail {
            label: (index == 0).then(|| label.to_owned()),
            cells,
        })
        .collect()
}

/// `text`, followed by `(note)` when `shown`.
fn noted(text: impl Display, note: &str, shown: bool) -> String {
    match shown {
        true => format!("{text} ({note})"),
        false => text.to_string(),
    }
}

fn dash() -> Cell {
    Cell::new("—", Tone::Muted)
}

fn id(id: impl Display) -> Cell {
    Cell::new(format!("#{id}"), Tone::Muted)
}

fn ids(ids: impl Iterator<Item = impl Display>) -> String {
    let ids: Vec<String> = ids.map(|id| format!("#{id}")).collect();
    if ids.is_empty() {
        "—".to_owned()
    } else {
        ids.join(" ")
    }
}

fn optional(value: Option<impl Display>) -> Cell {
    value.map_or_else(dash, |value| words(value).into())
}

fn status(status: Status) -> Cell {
    let tone = match status {
        Status::Validated | Status::Noted | Status::Reported => Tone::Success,
        Status::Shortlisted => Tone::Accent,
        Status::Unproven => Tone::Warning,
        Status::Identified => Tone::Normal,
        Status::BelowThreshold | Status::Rejected | Status::Merged => Tone::Muted,
    };
    Cell::new(words(status), tone)
}

fn confidence(confidence: Option<ConfidenceTier>) -> Cell {
    let Some(confidence) = confidence else {
        return dash();
    };
    let tone = match confidence {
        ConfidenceTier::Demonstrated => Tone::Success,
        ConfidenceTier::Confirmed => Tone::Accent,
        ConfidenceTier::Established | ConfidenceTier::Plausible | ConfidenceTier::Conjectured => {
            Tone::Muted
        }
    };
    Cell::new(words(confidence), tone)
}

/// A candidate's status with the test open on it: proving while its job runs, abandoned
/// once the job ended without settling it.
fn testing(status: Status, live: bool) -> Cell {
    match live {
        true => Cell::new(format!("{} · proving", words(status)), Tone::Accent),
        false => Cell::new(format!("{} · test abandoned", words(status)), Tone::Warning),
    }
}

/// A plan unit's standing outcome, or open.
fn unit_state(outcome: Option<Outcome>) -> Cell {
    match outcome {
        None => Cell::new("open", Tone::Accent),
        Some(Outcome::Completed) => Cell::new(words(Outcome::Completed), Tone::Success),
        Some(outcome) => Cell::new(words(outcome), Tone::Warning),
    }
}

/// A condition by its resolution at its current revision; without one it is open.
fn standing(standing: Option<ResolutionKind>) -> Cell {
    match standing {
        None => Cell::new("? open", Tone::Warning),
        Some(ResolutionKind::Holds) => Cell::new("✓ holds", Tone::Success),
        Some(ResolutionKind::Fails) => Cell::new("✗ fails", Tone::Error),
        Some(ResolutionKind::Asked) => Cell::new("? asked", Tone::Accent),
    }
}

/// Who a condition belongs to.
fn owner(on: &PremiseOn) -> String {
    match on {
        PremiseOn::Deployment { deployment } => format!("deployment #{deployment}"),
        PremiseOn::Flaw { id } => format!("flaw #{id}"),
        PremiseOn::Candidate { id, .. } => format!("candidate #{id}"),
    }
}

/// A finding's standing; only a current one is in `reports/`.
fn finding_status(status: FindingStatus) -> Cell {
    let tone = match status {
        FindingStatus::Current => Tone::Success,
        FindingStatus::Stale => Tone::Accent,
        FindingStatus::Superseded { .. } => Tone::Muted,
        FindingStatus::Withdrawn => Tone::Warning,
    };
    Cell::new(status.to_string(), tone)
}

fn verdict(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Validated => "validated".to_owned(),
        Verdict::Disproved => "rejected (disproved)".to_owned(),
        Verdict::PolicyExcluded { .. } => "rejected (policy excluded)".to_owned(),
        Verdict::Unproven => "unproven".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use skyhook_cli::page::Toggle;

    use super::*;
    use crate::store::tests::{component, fixture, new_flaw, path, shortlisted, validated};

    /// Sections are read only while open: an open section lists its records in its own
    /// order, with detail for the record opened, and a closed one only its total.
    #[test]
    fn open_sections_list_records_in_order_with_detail_for_open_records() {
        let (_session, store, actor) = fixture();
        let shortlisted = shortlisted(&store, &actor);
        let validated = validated(&store, &actor);
        let mut open = Disclosed::default();
        open.apply(&Toggle::Section(Shelf::Candidates.key()));
        open.apply(&Toggle::Record(
            Shelf::Candidates.key(),
            RecordKey(shortlisted.to_string()),
        ));
        let blocks = sections(
            &store,
            &store.run_state().unwrap(),
            &store.coverage().unwrap(),
            &open,
            &[],
        )
        .unwrap();
        let sections: Vec<&Section> = (blocks.iter())
            .map(|block| match block {
                Block::Section(section) => section,
                _ => panic!("only sections"),
            })
            .collect();
        let candidates = sections[0];
        let keys: Vec<&str> = (candidates.records.iter())
            .map(|record| record.key.0.as_str())
            .collect();
        assert_eq!(keys, [validated.to_string(), shortlisted.to_string()]);
        assert!(candidates.records[0].detail.is_none());
        let detail = candidates.records[1].detail.as_ref().unwrap();
        assert!(
            detail
                .iter()
                .any(|line| line.label.as_deref() == Some("assessed"))
        );
        let flaws = sections[4];
        assert_eq!((flaws.key, flaws.total), (Shelf::Flaws.key(), 2));
        assert!(flaws.records.is_empty());
    }

    /// Paths still owing review lead the section, then those reaching nothing recorded,
    /// then checked ones; an open path shows what it still owes and what it decided.
    #[test]
    fn paths_list_unchecked_first_with_their_work_and_decisions() {
        let (_session, store, actor) = fixture();
        let candidate = shortlisted(&store, &actor);
        let checked = store.candidate(candidate).unwrap().paths[0];
        let parser = component(&store, &actor, "parser");
        store.add_flaw(&actor, &new_flaw(parser, "length")).unwrap();
        let unchecked = path(&store, &actor, "upload", &[parser]);
        let unreached = path(&store, &actor, "admin", &[]);
        let mut open = Disclosed::default();
        open.apply(&Toggle::Section(Shelf::Paths.key()));
        for path in [unchecked, checked] {
            open.apply(&Toggle::Record(
                Shelf::Paths.key(),
                RecordKey(path.to_string()),
            ));
        }
        let blocks = sections(
            &store,
            &store.run_state().unwrap(),
            &store.coverage().unwrap(),
            &open,
            &[],
        )
        .unwrap();
        let Block::Section(paths) = &blocks[2] else {
            panic!("the paths section")
        };
        let keys: Vec<&str> = (paths.records.iter())
            .map(|record| record.key.0.as_str())
            .collect();
        assert_eq!(
            keys,
            [unchecked.to_string(), unreached.to_string(), "1".to_owned()]
        );
        let line = |record: &Record, label: &str| {
            let detail = record.detail.as_ref().unwrap();
            (detail
                .iter()
                .find(|line| line.label.as_deref() == Some(label)))
            .cloned()
        };
        let owed = line(&paths.records[0], "owed").unwrap();
        assert_eq!(owed.cells[0].tone, Tone::Warning, "decisions are owed");
        assert!(paths.records[1].detail.is_none());
        assert!(line(&paths.records[2], "exposures").is_some());
    }
}
