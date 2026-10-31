//! The Research tab's record sections: what the run holds of each kind, the first of them
//! once a section is open, and the full record once the viewer opens it.
use std::fmt::Display;

use chrono::{DateTime, Utc};
use skyhook::job::JobState;
use skyhook_cli::page::{
    Block, Cell, Column, Detail, Disclosed, Record, RecordDetail, RecordKey, Section, SectionKey,
    Tone,
};

use super::words;
use crate::store::{
    CandidateView, EvidenceView, FlawStatus, LastTask, NodeId, NodeKind, NodeStatus, NodeView,
    OpenTest, Owing, PathStatus, PremiseRef, PremiseStatus, RunState, ShelfRow, Status, Store,
    TaskDetail, TaskId,
};

type Result<T> = std::result::Result<T, crate::store::Error>;

/// The kinds of record the tab lists, in reading order.
#[derive(Clone, Copy)]
enum Shelf {
    Nodes(NodeKind),
    Tasks,
}

const SHELVES: [Shelf; 8] = [
    Shelf::Nodes(NodeKind::Candidate),
    Shelf::Nodes(NodeKind::Finding),
    Shelf::Nodes(NodeKind::Path),
    Shelf::Tasks,
    Shelf::Nodes(NodeKind::Flaw),
    Shelf::Nodes(NodeKind::Premise),
    Shelf::Nodes(NodeKind::Component),
    Shelf::Nodes(NodeKind::Source),
];

impl Shelf {
    fn key(self) -> SectionKey {
        SectionKey(match self {
            Self::Nodes(NodeKind::Candidate) => "candidates",
            Self::Nodes(NodeKind::Finding) => "findings",
            Self::Nodes(NodeKind::Path) => "paths",
            Self::Tasks => "tasks",
            Self::Nodes(NodeKind::Flaw) => "flaws",
            Self::Nodes(NodeKind::Premise) => "premises",
            Self::Nodes(NodeKind::Component) => "components",
            Self::Nodes(NodeKind::Source) => "sources",
        })
    }

    fn title(self) -> &'static str {
        match self {
            Self::Nodes(NodeKind::Candidate) => "Candidates",
            Self::Nodes(NodeKind::Finding) => "Findings",
            Self::Nodes(NodeKind::Path) => "Paths",
            Self::Tasks => "Tasks",
            Self::Nodes(NodeKind::Flaw) => "Flaws",
            Self::Nodes(NodeKind::Premise) => "Premises",
            Self::Nodes(NodeKind::Component) => "Components",
            Self::Nodes(NodeKind::Source) => "Sources",
        }
    }

    fn columns(self) -> &'static [(&'static str, u8)] {
        match self {
            Self::Tasks => &[
                ("id", 0),
                ("phase", 0),
                ("subjects", 2),
                ("state", 0),
                ("began", 1),
                ("ended", 3),
            ],
            Self::Nodes(NodeKind::Component | NodeKind::Source | NodeKind::Finding) => {
                &[("id", 0), ("title", 0)]
            }
            Self::Nodes(_) => &[
                ("id", 0),
                ("status", 0),
                ("owes", 1),
                ("last task", 2),
                ("title", 0),
            ],
        }
    }
}

/// Every section in reading order, with records and detail for what `open` shows; a
/// candidate with an open test shows it as proving, or abandoned once its job ended.
pub fn sections(store: &Store, state: &RunState, open: &Disclosed) -> Result<Vec<Block>> {
    let synthesis = &state.facts.synthesis;
    let coverage = &state.coverage;
    let candidates: u64 = coverage.candidates.values().sum();
    let mut blocks = Vec::with_capacity(SHELVES.len());
    for shelf in SHELVES {
        let key = shelf.key();
        let summary = match shelf {
            Shelf::Nodes(NodeKind::Candidate) => Some(format!(
                "{candidates} total · {} proving",
                state.open_tests.iter().filter(|test| test.live).count()
            )),
            Shelf::Nodes(NodeKind::Path) => Some(format!(
                "{} swept · {} unswept",
                coverage.swept,
                coverage.paths - coverage.swept
            )),
            Shelf::Tasks => Some(format!(
                "{} running · {} exhausted",
                synthesis.running.values().sum::<u64>(),
                synthesis.exhausted.len()
            )),
            _ => None,
        };
        let (total, records) = match (shelf, open.limit(key)) {
            (Shelf::Tasks, limit) => (
                store.task_count()?,
                limit
                    .map(|limit| tasks(store, key, limit, open))
                    .transpose()?,
            ),
            (Shelf::Nodes(kind), Some(limit)) => {
                let read = store.shelf(kind, limit)?;
                let records = (read.rows.into_iter())
                    .map(|row| node_record(store, key, row, open, &state.open_tests))
                    .collect::<Result<_>>()?;
                (read.total, Some(records))
            }
            (Shelf::Nodes(kind), None) => (store.shelf(kind, 0)?.total, None),
        };
        blocks.push(Block::Section(Section {
            key,
            title: shelf.title().to_owned(),
            summary,
            total,
            columns: (shelf.columns().iter())
                .map(|(title, priority)| Column {
                    title: (*title).to_owned(),
                    priority: *priority,
                })
                .collect(),
            records: records.unwrap_or_default(),
        }));
    }
    Ok(blocks)
}

/// A record keyed by `id`, with its detail while it is open.
fn record(
    section: SectionKey,
    id: impl Display,
    cells: Vec<Cell>,
    open: &Disclosed,
    detail: impl FnOnce(usize) -> Result<RecordDetail>,
) -> Result<Record> {
    let key = RecordKey(id.to_string());
    let detail = (open.detail(section, &key)).map(detail).transpose()?;
    Ok(Record { key, cells, detail })
}

fn node_record(
    store: &Store,
    section: SectionKey,
    row: ShelfRow,
    open: &Disclosed,
    tests: &[OpenTest],
) -> Result<Record> {
    let node = row.node;
    let cells = match node.status {
        None => vec![id(node.id), node.title.into()],
        Some(status) => {
            let test = tests
                .iter()
                .find(|test| NodeId::from(test.candidate) == node.id);
            vec![
                id(node.id),
                standing(status, test),
                owing(&row.owing),
                last_task(row.last),
                node.title.into(),
            ]
        }
    };
    record(section, node.id, cells, open, |_| {
        Ok(node_detail(&store.node(node.id)?).into())
    })
}

fn tasks(
    store: &Store,
    section: SectionKey,
    limit: usize,
    open: &Disclosed,
) -> Result<Vec<Record>> {
    (store.tasks(limit)?.into_iter())
        .map(|task| {
            let subjects = ids(task.subjects.iter());
            let cells = vec![
                id(task.id),
                words(task.phase).into(),
                subjects.into(),
                job_state(task.state),
                at(task.began),
                task.ended.map_or_else(dash, at),
            ];
            record(section, task.id, cells, open, |events| {
                task_detail(store, task.id, events)
            })
        })
        .collect()
}

/// A task's brief, its child's reply, and the first `events` records its child made.
fn task_detail(store: &Store, task: TaskId, events: usize) -> Result<RecordDetail> {
    let TaskDetail {
        brief,
        reply,
        events,
        more,
        ..
    } = store.task(task, events)?;
    let mut lines = vec![
        line("brief", vec![brief.map_or_else(dash, Cell::from)]),
        line(
            "reply",
            vec![reply.map_or_else(|| Cell::new("none yet", Tone::Muted), Cell::from)],
        ),
    ];
    lines.extend(listed(
        "records",
        events.into_iter().map(|event| {
            let nodes = ids(event.nodes.iter().map(|mark| mark.node));
            let mut cells = vec![
                at(event.at),
                Cell::new(words(event.kind), Tone::Accent),
                Cell::new(nodes, Tone::Muted),
            ];
            cells.extend(event.note.map(Cell::from));
            cells
        }),
    ));
    Ok(RecordDetail { lines, more })
}

/// A node's body, its own fields and its neighbours.
fn node_detail(view: &NodeView) -> Vec<Detail> {
    let head = view.head();
    let mut lines = vec![line("body", vec![head.body.clone().into()])];
    if let Some(into) = head.merged_into {
        lines.push(line("merged into", vec![id(into)]));
    }
    match view {
        NodeView::Component(component) => {
            lines.push(line("files", vec![component.files.join(" ").into()]));
            lines.push(line(
                "paths",
                vec![ids(component.paths.iter().map(|p| p.id)).into()],
            ));
            lines.extend(listed(
                "flaws",
                (component.flaws.iter()).map(|flaw| vec![id(flaw.id), flaw_status(flaw.status)]),
            ));
        }
        NodeView::Path(path) => {
            lines.push(line(
                "swept",
                vec![
                    path.swept
                        .map_or_else(dash, |event| format!("event #{event}").into()),
                ],
            ));
            lines.extend(listed(
                "steps",
                (path.steps.iter()).map(|step| {
                    vec![
                        Cell::new(format!("{}.", step.position + 1), Tone::Muted),
                        id(step.component.id),
                        step.location
                            .as_ref()
                            .map_or_else(dash, |at| at.to_string().into()),
                        step.note.clone().into(),
                    ]
                }),
            ));
            lines.push(line(
                "sources",
                vec![ids(path.sources.iter().map(|s| s.id)).into()],
            ));
            lines.extend(premises("premises", &path.premises));
            lines.extend(listed(
                "candidates",
                (path.candidates.iter()).map(|c| vec![id(c.id), status(c.status)]),
            ));
        }
        NodeView::Source(source) => {
            lines.push(line("entry", vec![source.location.to_string().into()]));
            lines.push(line("trust", vec![words(source.trust).into()]));
            lines.push(line(
                "paths",
                vec![ids(source.paths.iter().map(|p| p.id)).into()],
            ));
        }
        NodeView::Premise(premise) => {
            lines.push(line(
                "status",
                vec![
                    premise_status(premise.status),
                    words(premise.scope).into(),
                    match premise.disputed {
                        true => Cell::new("disputed", Tone::Warning),
                        false => dash(),
                    },
                ],
            ));
            if let Some(location) = &premise.location {
                lines.push(line("at", vec![location.to_string().into()]));
            }
            lines.push(line(
                "flaws",
                vec![ids(premise.flaws.iter().map(|f| f.id)).into()],
            ));
            lines.push(line(
                "paths",
                vec![ids(premise.paths.iter().map(|p| p.id)).into()],
            ));
        }
        NodeView::Flaw(flaw) => {
            lines.push(line(
                "at",
                vec![id(flaw.component.id), flaw.location.to_string().into()],
            ));
            lines.push(line(
                "status",
                vec![
                    flaw_status(flaw.status),
                    flaw.cwe
                        .map_or_else(dash, |cwe| format!("CWE-{cwe}").into()),
                ],
            ));
            lines.extend(premises("premises", &flaw.premises));
            lines.push(line(
                "candidates",
                vec![ids(flaw.candidates.iter().map(|c| c.id)).into()],
            ));
            if let Some(finding) = flaw.finding {
                lines.push(line("finding", vec![id(finding)]));
            }
        }
        NodeView::Candidate(candidate) => lines.extend(candidate_detail(candidate)),
        NodeView::Finding(finding) => {
            lines.push(line(
                "report",
                vec![match finding.current {
                    true => Cell::new(format!("reports/{}/", finding.root.id), Tone::Success),
                    false => Cell::new("stale", Tone::Warning),
                }],
            ));
            lines.push(line("root", vec![id(finding.root.id)]));
            lines.extend(listed(
                "scenarios",
                (finding.scenarios.iter()).map(|scenario| {
                    vec![
                        id(scenario.candidate.id),
                        status(scenario.candidate.status),
                        words(scenario.severity).into(),
                        scenario
                            .confidence
                            .map_or_else(dash, |tier| words(tier).into()),
                    ]
                }),
            ));
        }
    }
    lines.extend(evidence(&head.evidence));
    lines
}

fn candidate_detail(candidate: &CandidateView) -> Vec<Detail> {
    let mut lines = vec![line(
        "status",
        vec![
            status(candidate.status),
            candidate
                .confidence
                .map_or_else(dash, |tier| words(tier).into()),
        ],
    )];
    lines.push(line(
        "path",
        vec![id(candidate.path.id), candidate.path.title.clone().into()],
    ));
    for (position, link) in candidate.chain.iter().enumerate() {
        lines.push(Detail {
            label: (position == 0).then(|| "chain".to_owned()),
            cells: vec![
                Cell::new(format!("{}.", position + 1), Tone::Muted),
                id(link.flaw.id),
                flaw_status(link.flaw.status),
                link.flaw.title.clone().into(),
            ],
        });
        lines.extend(premises("", &link.premises));
    }
    lines.extend(premises("path premises", &candidate.path_premises));
    let stale = |current: bool| (!current).then(|| Cell::new("stale", Tone::Warning));
    if let Some(assessment) = &candidate.assessment {
        let mut cells = vec![
            words(assessment.severity).into(),
            format!("{:.1}", assessment.score).into(),
            Cell::new(assessment.vector.clone(), Tone::Muted),
        ];
        cells.extend(stale(assessment.current));
        lines.push(line("assessed", cells));
        lines.extend(
            assessment
                .note
                .clone()
                .map(|note| line("", vec![note.into()])),
        );
    }
    if let Some(verdict) = &candidate.verdict {
        let mut cells = vec![words(verdict.verdict).into()];
        cells.extend(stale(verdict.current));
        cells.extend(verdict.note.clone().map(Cell::from));
        lines.push(line("verdict", cells));
    }
    lines.extend(listed(
        "tests",
        (candidate.tests.iter()).map(|test| {
            let mut cells = vec![id(test.id), words(test.result).into()];
            cells.extend(test.note.clone().map(Cell::from));
            cells
        }),
    ));
    lines.push(line(
        "environments",
        vec![ids(candidate.environments.iter().map(|e| e.id)).into()],
    ));
    lines
}

fn premises(label: &str, premises: &[PremiseRef]) -> Vec<Detail> {
    listed(
        label,
        (premises.iter()).map(|premise| {
            vec![
                id(premise.id),
                premise_status(premise.status),
                Cell::new(words(premise.scope), Tone::Muted),
                premise.title.clone().into(),
            ]
        }),
    )
}

fn evidence(evidence: &[EvidenceView]) -> Vec<Detail> {
    listed(
        "evidence",
        (evidence.iter()).map(|item| {
            vec![
                Cell::new(words(item.stance), Tone::Muted),
                item.location
                    .as_ref()
                    .map_or_else(dash, |at| at.to_string().into()),
                item.note.clone().into(),
            ]
        }),
    )
}

fn line(label: &str, cells: Vec<Cell>) -> Detail {
    Detail {
        label: (!label.is_empty()).then(|| label.to_owned()),
        cells,
    }
}

/// One line per item, the first labelled.
fn listed(label: &str, items: impl Iterator<Item = Vec<Cell>>) -> Vec<Detail> {
    (items.enumerate())
        .map(|(index, cells)| Detail {
            label: (index == 0 && !label.is_empty()).then(|| label.to_owned()),
            cells,
        })
        .collect()
}

fn dash() -> Cell {
    Cell::new("—", Tone::Muted)
}

fn id(id: impl Display) -> Cell {
    Cell::new(format!("#{id}"), Tone::Muted)
}

fn ids(ids: impl Iterator<Item = impl Display>) -> String {
    let ids: Vec<String> = ids.map(|id| format!("#{id}")).collect();
    match ids.is_empty() {
        true => "—".to_owned(),
        false => ids.join(" "),
    }
}

fn at(at: DateTime<Utc>) -> Cell {
    Cell::new(at.format("%Y-%m-%d %H:%M").to_string(), Tone::Muted)
}

/// The phases a node owes now: at work, or exhausted until what it rests on changes.
fn owing(owing: &[Owing]) -> Cell {
    if owing.is_empty() {
        return dash();
    }
    let text: Vec<String> = (owing.iter())
        .map(|owing| match owing.exhausted {
            true => format!("{} (exhausted)", owing.phase),
            false => owing.phase.to_string(),
        })
        .collect();
    let tone = match owing.iter().any(|owing| owing.exhausted) {
        true => Tone::Warning,
        false => Tone::Accent,
    };
    Cell::new(text.join(", "), tone)
}

/// A job's state: live is at work, completed succeeded, any other end fell short.
fn job_state(state: JobState) -> Cell {
    let tone = match state {
        JobState::Completed => Tone::Success,
        state if state.is_terminal() => Tone::Warning,
        _ => Tone::Accent,
    };
    Cell::new(words(state), tone)
}

/// A node's latest task, by its id, phase and its job's state.
fn last_task(task: Option<LastTask>) -> Cell {
    task.map_or_else(dash, |task| {
        let cell = job_state(task.state);
        Cell::new(
            format!("#{} {} {}", task.id, task.phase, cell.text),
            cell.tone,
        )
    })
}

/// A node's own status; a candidate with an open test shows it as proving, or abandoned
/// once its job ended.
fn standing(status: NodeStatus, test: Option<&OpenTest>) -> Cell {
    match (status, test) {
        (NodeStatus::Candidate(status), Some(test)) => {
            let (note, tone) = match test.live {
                true => ("proving", Tone::Accent),
                false => ("test abandoned", Tone::Warning),
            };
            Cell::new(format!("{} ({note})", words(status)), tone)
        }
        (NodeStatus::Candidate(candidate), None) => self::status(candidate),
        (NodeStatus::Path(path), _) => match path {
            PathStatus::Swept => Cell::new("swept", Tone::Success),
            PathStatus::Unswept => Cell::new("unswept", Tone::Accent),
        },
        (NodeStatus::Premise(premise), _) => premise_status(premise),
        (NodeStatus::Flaw(flaw), _) => flaw_status(flaw),
    }
}

fn status(status: Status) -> Cell {
    let tone = match status {
        Status::Validated | Status::Noted => Tone::Success,
        Status::Shortlisted => Tone::Accent,
        Status::Unproven => Tone::Warning,
        Status::Identified
        | Status::BelowThreshold
        | Status::Disproved
        | Status::Excluded
        | Status::Merged => Tone::Muted,
    };
    Cell::new(words(status), tone)
}

fn premise_status(status: PremiseStatus) -> Cell {
    let tone = match status {
        PremiseStatus::Holds => Tone::Success,
        PremiseStatus::Fails => Tone::Warning,
        PremiseStatus::Undecided => Tone::Accent,
    };
    Cell::new(words(status), tone)
}

fn flaw_status(status: FlawStatus) -> Cell {
    let tone = match status {
        FlawStatus::Suspected => Tone::Accent,
        FlawStatus::Contained | FlawStatus::Refuted => Tone::Muted,
    };
    Cell::new(words(status), tone)
}

#[cfg(test)]
mod tests {
    use skyhook_cli::page::Toggle;

    use super::*;
    use crate::store::{
        Begin, Link, PremiseScope, Verdict,
        tests::{
            HIGH, assess, begun, component, fixture, job_state as set_job, path, premise, scenario,
            sweep, verdict,
        },
    };

    fn section(blocks: &[Block], shelf: Shelf) -> &Section {
        (blocks.iter())
            .find_map(|block| match block {
                Block::Section(section) if section.key == shelf.key() => Some(section),
                _ => None,
            })
            .unwrap()
    }

    fn opened(store: &Store, open: &Disclosed) -> Vec<Block> {
        sections(store, &store.run_state().unwrap(), open).unwrap()
    }

    /// Sections are read only while open: an open section lists its records in its own
    /// order, with detail for the record opened, marking a result that no longer stands, and a
    /// closed one only its total.
    #[test]
    fn open_sections_list_records_in_order_with_detail_for_open_records() {
        let (_session, store, actor) = fixture();
        let [first, second] = ["decoder", "parser"].map(|name| scenario(&store, &actor, name));
        assess(&store, &actor, second.candidate, HIGH);
        verdict(&store, &actor, second.candidate, Verdict::Validated);
        let rests = Link::FlawPremise {
            flaw: second.flaw,
            premise: premise(&store, &actor, PremiseScope::Code),
        };
        store.link(&actor, &[rests]).unwrap();
        let candidates = Shelf::Nodes(NodeKind::Candidate);
        let mut open = Disclosed::default();
        open.apply(&Toggle::Section(candidates.key()));
        open.apply(&Toggle::Record(
            candidates.key(),
            RecordKey(second.candidate.to_string()),
        ));
        let blocks = opened(&store, &open);
        let keys: Vec<Vec<&str>> = (blocks.iter())
            .map(|block| match block {
                Block::Section(section) => {
                    section.records.iter().map(|r| r.key.0.as_str()).collect()
                }
                _ => panic!("only sections"),
            })
            .collect();
        let (first_key, second_key) = (first.candidate.to_string(), second.candidate.to_string());
        assert_eq!(keys[0], [first_key.as_str(), second_key.as_str()]);
        assert!(keys[1..].iter().all(Vec::is_empty));
        let records = &section(&blocks, candidates).records;
        assert!(records[0].detail.is_none());
        let detail = records[1].detail.as_ref().unwrap();
        let labels: Vec<_> = (detail.lines.iter())
            .filter_map(|line| line.label.as_deref())
            .collect();
        let stale = |label: &str| {
            (detail.lines.iter())
                .find(|line| line.label.as_deref() == Some(label))
                .unwrap_or_else(|| panic!("{label} in {labels:?}"))
                .cells
                .iter()
                .any(|cell| cell.text == "stale")
        };
        assert_eq!(
            (stale("assessed"), stale("verdict")),
            (false, true),
            "a premise linked since stales the verdict alone"
        );
        let flaws = section(&blocks, Shelf::Nodes(NodeKind::Flaw));
        assert_eq!(flaws.total, 2);
    }

    /// Paths list unswept first, each with the work it owes and its latest task.
    #[test]
    fn paths_list_unswept_first_with_their_work() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let [first, second, third] =
            ["stream", "file", "pipe"].map(|name| path(&store, &actor, name, &[decoder]));
        sweep(&store, first);
        let (_, _, task) = begun(&store, &Begin::Sweep(vec![second]));
        let paths = Shelf::Nodes(NodeKind::Path);
        let mut open = Disclosed::default();
        open.apply(&Toggle::Section(paths.key()));
        let blocks = opened(&store, &open);
        let rows: Vec<Vec<String>> = (section(&blocks, paths).records.iter())
            .map(|record| record.cells.iter().map(|cell| cell.text.clone()).collect())
            .collect();
        assert_eq!(
            rows,
            [
                vec![
                    format!("#{second}"),
                    "unswept".to_owned(),
                    "—".to_owned(),
                    format!("#{task} sweep running"),
                    "file".to_owned()
                ],
                vec![
                    format!("#{third}"),
                    "unswept".to_owned(),
                    "sweep".to_owned(),
                    "—".to_owned(),
                    "pipe".to_owned()
                ],
                vec![
                    format!("#{first}"),
                    "swept".to_owned(),
                    "—".to_owned(),
                    "—".to_owned(),
                    "stream".to_owned()
                ],
            ]
        );
    }

    /// Tasks list newest first by their job's state, and an open one shows its brief, its
    /// reply and what its child recorded.
    #[test]
    fn tasks_list_newest_first_by_their_jobs_state() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let stream = path(&store, &actor, "stream", &[decoder]);
        let work = Begin::Sweep(vec![stream]);
        let (_, job, failed) = begun(&store, &work);
        set_job(&store, job, JobState::Failed);
        let (_, _, running) = begun(&store, &work);
        let mut open = Disclosed::default();
        open.apply(&Toggle::Section(Shelf::Tasks.key()));
        open.apply(&Toggle::Record(
            Shelf::Tasks.key(),
            RecordKey(failed.to_string()),
        ));
        let blocks = opened(&store, &open);
        let tasks = section(&blocks, Shelf::Tasks);
        let rows: Vec<(&str, Tone)> = (tasks.records.iter())
            .map(|record| (record.key.0.as_str(), record.cells[3].tone))
            .collect();
        let (running, failed) = (running.to_string(), failed.to_string());
        assert_eq!(
            (tasks.total, rows),
            (
                2,
                vec![
                    (running.as_str(), Tone::Accent),
                    (failed.as_str(), Tone::Warning)
                ]
            )
        );
        let detail = tasks.records[1].detail.as_ref().unwrap();
        let labels: Vec<_> = (detail.lines.iter())
            .filter_map(|line| line.label.as_deref())
            .collect();
        assert_eq!(labels, ["brief", "reply", "records"]);
        assert!(!detail.more);
    }
}
