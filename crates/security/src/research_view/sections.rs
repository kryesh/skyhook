//! The Research tab's record sections: what the run holds of each kind, the first of them
//! once a section is open, and the full record once the viewer opens it.
use std::fmt::Display;

use chrono::{DateTime, Utc};
use skyhook::job::JobState;
use skyhook::product::page::{
    Block, Cell, Column, Detail, Disclosed, Record, RecordDetail, RecordKey, Section, SectionKey,
    Tone,
};

use super::words;
use crate::cvss::{Cvss40, Severity};
use crate::store::{
    Assessed, Behind, CandidateView, ConfidenceTier, Currency, EvidenceView, FlawStatus,
    GroupMember, InventoryStatus, LastTask, ListedTask, LiveInput, Liveness, NodeId, NodeKind,
    NodeStatus, NodeView, OpenTest, Owing, PathStatus, PremiseRef, PremiseStatus, Proposal,
    RunState, ShelfRow, StateScoped, Status, Store, TaskDetail, TaskId,
};

type Result<T> = std::result::Result<T, crate::store::Error>;

/// The kinds of record the tab lists, in reading order.
#[derive(Clone, Copy)]
enum ShelfKind {
    Nodes(NodeKind),
    Tasks(Liveness),
}

const SHELVES: [ShelfKind; 9] = [
    ShelfKind::Nodes(NodeKind::Candidate),
    ShelfKind::Nodes(NodeKind::Finding),
    ShelfKind::Nodes(NodeKind::Path),
    ShelfKind::Tasks(Liveness::Live),
    ShelfKind::Tasks(Liveness::Ended),
    ShelfKind::Nodes(NodeKind::Flaw),
    ShelfKind::Nodes(NodeKind::Premise),
    ShelfKind::Nodes(NodeKind::Component),
    ShelfKind::Nodes(NodeKind::State),
];

impl ShelfKind {
    fn key(self) -> SectionKey {
        SectionKey(match self {
            Self::Nodes(NodeKind::Candidate) => "candidates",
            Self::Nodes(NodeKind::Finding) => "findings",
            Self::Nodes(NodeKind::Path) => "paths",
            Self::Tasks(Liveness::Live) => "running",
            Self::Tasks(Liveness::Ended) => "history",
            Self::Nodes(NodeKind::Flaw) => "flaws",
            Self::Nodes(NodeKind::Premise) => "premises",
            Self::Nodes(NodeKind::Component) => "components",
            Self::Nodes(NodeKind::State) => "states",
        })
    }

    fn title(self) -> &'static str {
        match self {
            Self::Nodes(NodeKind::Candidate) => "Candidates",
            Self::Nodes(NodeKind::Finding) => "Findings",
            Self::Nodes(NodeKind::Path) => "Paths",
            Self::Tasks(Liveness::Live) => "Running tasks",
            Self::Tasks(Liveness::Ended) => "Task history",
            Self::Nodes(NodeKind::Flaw) => "Flaws",
            Self::Nodes(NodeKind::Premise) => "Premises",
            Self::Nodes(NodeKind::Component) => "Components",
            Self::Nodes(NodeKind::State) => "States",
        }
    }

    fn columns(self) -> Vec<Column> {
        match self {
            Self::Tasks(liveness) => headings(task_fields(liveness)),
            Self::Nodes(kind) => headings(node_fields(kind)),
        }
    }
}

/// A section's column: its heading, its [`Column::priority`] and the cell it shows of each of
/// its rows, so a section's columns and its records' cells come from one list.
type ColumnOf<R> = (&'static str, u8, fn(&R) -> Cell);

fn headings<R>(fields: &[ColumnOf<R>]) -> Vec<Column> {
    (fields.iter())
        .map(|(title, priority, _)| Column {
            title: (*title).to_owned(),
            priority: *priority,
        })
        .collect()
}

fn cells<R>(fields: &[ColumnOf<R>], row: &R) -> Vec<Cell> {
    fields.iter().map(|(_, _, cell)| cell(row)).collect()
}

const TASK_ID: ColumnOf<ListedTask> = ("id", 0, |listed| id(listed.task.id));
const TASK_PHASE: ColumnOf<ListedTask> = ("phase", 0, |listed| words(listed.task.phase).into());
const TASK_STATE: ColumnOf<ListedTask> = ("state", 0, |listed| job_state(listed.task.state));
const TASK_SUBJECTS: ColumnOf<ListedTask> = ("subjects", 3, |listed| {
    ids(listed.task.subjects.iter()).into()
});
/// The brief's first line; the brief runs long, so it comes last, where the row's remaining
/// width clips it.
const TASK_BRIEF: ColumnOf<ListedTask> = ("brief", 0, |listed| {
    let first = |brief: &String| {
        brief
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(str::to_owned)
    };
    listed
        .brief
        .as_ref()
        .and_then(first)
        .map_or_else(dash, Cell::from)
});

fn task_fields(liveness: Liveness) -> &'static [ColumnOf<ListedTask>] {
    match liveness {
        Liveness::Live => &[
            TASK_ID,
            TASK_PHASE,
            TASK_STATE,
            ("began", 1, |listed| at(listed.task.began)),
            TASK_SUBJECTS,
            TASK_BRIEF,
        ],
        Liveness::Ended => &[
            TASK_ID,
            TASK_PHASE,
            TASK_STATE,
            ("ended", 1, |listed| listed.task.ended.map_or_else(dash, at)),
            TASK_SUBJECTS,
            TASK_BRIEF,
        ],
    }
}

/// A node's row, with the open test, the assessment and what a candidate waits behind.
struct NodeRow {
    row: ShelfRow,
    test: Option<OpenTest>,
    assessed: Option<Assessed>,
    behind: Option<Behind>,
}

const NODE_ID: ColumnOf<NodeRow> = ("id", 0, |node| id(node.row.node.id));
const NODE_STATUS: ColumnOf<NodeRow> = ("status", 0, |node| {
    let currency = node
        .assessed
        .map_or(Currency::Current, |assessed| assessed.currency);
    let cell =
        (node.row.node.status).map_or_else(dash, |status| standing(status, node.test, currency));
    match node.behind {
        Some(Behind::Noted { lead }) => Cell::new(format!("{} ↑{lead}", cell.text), cell.tone),
        _ => cell,
    }
});
const NODE_OWES: ColumnOf<NodeRow> = ("owes", 1, |node| {
    let cell = owing(&node.row.owing);
    let lead = match node.behind {
        Some(Behind::Test { lead } | Behind::PremiseTest { lead, .. }) => lead,
        _ => return cell,
    };
    let waiting = format!("test ↑{lead}");
    match node.row.owing.is_empty() {
        true => Cell::new(waiting, Tone::Muted),
        false => Cell::new(format!("{}, {waiting}", cell.text), cell.tone),
    }
});
const NODE_LAST: ColumnOf<NodeRow> = ("last task", 2, |node| last_task(node.row.last));
const NODE_TITLE: ColumnOf<NodeRow> = ("title", 0, |node| node.row.node.title.clone().into());
const NODE_SCORE: ColumnOf<NodeRow> = ("cvss", 0, |node| {
    (node.assessed).map_or_else(dash, |assessed| score(assessed.vector))
});

/// The columns of a section of `kind`'s nodes; a finding has no standing or work of its own,
/// and a candidate leads with its score.
fn node_fields(kind: NodeKind) -> &'static [ColumnOf<NodeRow>] {
    match kind {
        NodeKind::Finding => &[NODE_ID, NODE_TITLE],
        NodeKind::Candidate => &[
            NODE_ID,
            NODE_SCORE,
            NODE_STATUS,
            NODE_OWES,
            NODE_LAST,
            NODE_TITLE,
        ],
        _ => &[NODE_ID, NODE_STATUS, NODE_OWES, NODE_LAST, NODE_TITLE],
    }
}

/// A score in bold, coloured by its rating: critical red, high orange, medium yellow.
fn score(vector: Cvss40) -> Cell {
    let tone = match vector.severity() {
        Severity::Critical => Tone::Error,
        Severity::High => Tone::Caution,
        Severity::Medium => Tone::Warning,
        Severity::Low => Tone::Normal,
        Severity::None => Tone::Muted,
    };
    Cell::new(format!("{:.1}", vector.score()), tone).strong()
}

/// Every section in reading order, with records and detail for what `open` shows; a
/// candidate with an open test shows it as proving, or abandoned once its job ended.
pub(crate) fn sections(store: &Store, state: &RunState, open: &Disclosed) -> Result<Vec<Block>> {
    let summary = &state.facts.summary;
    let coverage = &state.coverage;
    let candidates: u64 = coverage.candidates.values().sum();
    let mut blocks = Vec::with_capacity(SHELVES.len());
    for shelf in SHELVES {
        let key = shelf.key();
        let summary = match shelf {
            ShelfKind::Nodes(NodeKind::Candidate) => Some(format!(
                "{candidates} total · {} proving",
                state.open_tests.iter().filter(|test| test.live).count()
            )),
            ShelfKind::Nodes(NodeKind::Path) => Some(format!(
                "{} swept · {} unswept",
                coverage.swept,
                coverage.paths - coverage.swept
            )),
            ShelfKind::Tasks(Liveness::Live) => Some(format!(
                "{} exhausted",
                summary.exhausted.values().sum::<u64>()
            )),
            _ => None,
        };
        let (total, records) = match (shelf, open.limit(key)) {
            (ShelfKind::Tasks(liveness), limit) => (
                store.task_count(liveness)?,
                limit
                    .map(|limit| tasks(store, (key, liveness), limit, open))
                    .transpose()?,
            ),
            (ShelfKind::Nodes(kind), Some(limit)) => {
                let read = store.shelf(kind, limit)?;
                let (assessments, behind) = match kind {
                    NodeKind::Candidate => {
                        let ids: Vec<NodeId> = read.rows.iter().map(|row| row.node.id).collect();
                        (store.assessments(&ids)?, store.behind(&ids)?)
                    }
                    _ => Default::default(),
                };
                let records = (read.rows.into_iter())
                    .map(|row| {
                        let id = row.node.id;
                        let extra = (assessments.get(&id).copied(), behind.get(&id).copied());
                        node_record(store, (key, kind), (row, extra), open, &state.open_tests)
                    })
                    .collect::<Result<_>>()?;
                (read.total, Some(records))
            }
            (ShelfKind::Nodes(kind), None) => (store.shelf(kind, 0)?.total, None),
        };
        blocks.push(Block::Section(Section {
            key,
            title: shelf.title().to_owned(),
            summary,
            total,
            columns: shelf.columns(),
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
    (section, kind): (SectionKey, NodeKind),
    (row, (assessed, behind)): (ShelfRow, (Option<Assessed>, Option<Behind>)),
    open: &Disclosed,
    tests: &[OpenTest],
) -> Result<Record> {
    let node = row.node.id;
    let test = (tests.iter())
        .find(|test| NodeId::from(test.candidate) == node)
        .copied();
    let row = NodeRow {
        row,
        test,
        assessed,
        behind,
    };
    let cells = cells(node_fields(kind), &row);
    record(section, node, cells, open, |_| {
        let mut lines = node_detail(&store.node(node)?);
        if kind == NodeKind::Candidate {
            lines.extend(group_detail(&store.root_group(node)?, behind));
        }
        Ok(lines.into())
    })
}

fn tasks(
    store: &Store,
    (section, liveness): (SectionKey, Liveness),
    limit: usize,
    open: &Disclosed,
) -> Result<Vec<Record>> {
    (store.tasks(liveness, limit)?.into_iter())
        .map(|listed| {
            let task = listed.task.id;
            let cells = cells(task_fields(liveness), &listed);
            record(section, task, cells, open, |events| {
                task_detail(store, task, events)
            })
        })
        .collect()
}

/// A task's subjects in full, its brief, its child's reply, and the first `events` records its
/// child made.
fn task_detail(store: &Store, task: TaskId, events: usize) -> Result<RecordDetail> {
    let TaskDetail {
        task,
        brief,
        reply,
        events,
        more,
        ..
    } = store.task(task, events)?;
    let mut lines = vec![
        line("subjects", vec![ids(task.subjects.iter()).into()]),
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
            lines.push(line("status", vec![inventory_status(component.status)]));
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
            lines.push(line("status", vec![path_status(path.status)]));
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
            lines.extend(listed(
                "states",
                (path.states.iter()).map(|state| {
                    vec![
                        id(state.id),
                        words(state.access).into(),
                        state.location.to_string().into(),
                    ]
                }),
            ));
            lines.extend(premises("premises", &path.premises));
            lines.extend(listed(
                "candidates",
                (path.candidates.iter()).map(|c| vec![id(c.id), status(c.status)]),
            ));
        }
        NodeView::State(state) => {
            lines.push(line(
                "status",
                vec![
                    inventory_status(state.status),
                    words(state.scope.kind()).into(),
                ],
            ));
            if let StateScoped::Input(LiveInput { read: Some(input) }) = &state.scope {
                lines.push(line("read at", vec![input.location.to_string().into()]));
                lines.push(line("trust", vec![words(input.trust).into()]));
            }
            lines.extend(listed(
                "accesses",
                (state.accesses.iter()).map(|access| {
                    vec![
                        id(access.path.id),
                        words(access.access).into(),
                        access.location.to_string().into(),
                    ]
                }),
            ));
            lines.extend(listed(
                "left wrong by",
                (state.flaws.iter()).map(|flaw| vec![id(flaw.id), flaw_status(flaw.status)]),
            ));
            if let Some(check) = &state.check {
                lines.push(line(
                    "checked",
                    vec![
                        id(check.event),
                        match check.current {
                            true => Cell::new("current", Tone::Success),
                            false => Cell::new("stale", Tone::Warning),
                        },
                    ],
                ));
            }
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
            lines.extend((premise.proposed.as_ref()).map(|p| proposed(premise_status(p.value), p)));
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
            lines.extend((flaw.proposed.as_ref()).map(|p| proposed(flaw_status(p.value), p)));
            lines.extend(premises("premises", &flaw.premises));
            lines.push(line(
                "leaves wrong",
                vec![ids(flaw.states.iter().map(|s| s.id)).into()],
            ));
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
    if !head.tasks.is_empty() {
        lines.push(line("tasks", vec![ids(head.tasks.iter()).into()]));
    }
    lines
}

/// A candidate's root group, most severe first, and what it waits behind.
fn group_detail(members: &[GroupMember], behind: Option<Behind>) -> Vec<Detail> {
    let member = |member: &GroupMember| {
        let severity = member
            .severity
            .map_or_else(|| "unassessed".to_owned(), words);
        let state = match member.confidence {
            Some(tier @ ConfidenceTier::Demonstrated) => words(tier),
            _ => words(member.state),
        };
        format!("#{} {severity}, {state}", member.candidate)
    };
    let mut lines = vec![line(
        "root group",
        vec![
            members
                .iter()
                .map(member)
                .collect::<Vec<_>>()
                .join(" · ")
                .into(),
        ],
    )];
    let waiting = match behind {
        Some(Behind::Noted { lead }) => Some(format!("#{lead} reports this root")),
        Some(Behind::Test { lead }) => Some(format!("#{lead}'s test, its group's first")),
        Some(Behind::PremiseTest { lead, premise }) => {
            Some(format!("#{lead}'s test of premise #{premise}"))
        }
        None => None,
    };
    lines.extend(waiting.map(|text| line("waiting on", vec![text.into()])));
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
    for (position, action) in candidate.actions.iter().enumerate() {
        lines.push(Detail {
            label: (position == 0).then(|| "actions".to_owned()),
            cells: vec![
                Cell::new(format!("{}.", position + 1), Tone::Muted),
                id(action.path.id),
                action.path.title.clone().into(),
            ],
        });
        lines.extend(premises("", &action.premises));
    }
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
    lines.extend((candidate.proposed_assessment.as_ref()).map(|p| {
        let value = format!("{} {:.1}", words(p.value.severity), p.value.score);
        proposed(value.into(), p)
    }));
    if let Some(verdict) = &candidate.verdict {
        let mut cells = vec![words(verdict.verdict).into()];
        cells.extend(stale(verdict.current));
        cells.extend(verdict.note.clone().map(Cell::from));
        lines.push(line("verdict", cells));
    }
    lines.extend((candidate.proposed_verdict.as_ref()).map(|p| proposed(words(p.value).into(), p)));
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

/// A result its phase has yet to decide: its value, then the event, task and agent recording it.
fn proposed<T>(value: Cell, proposal: &Proposal<T>) -> Detail {
    let mut cells = vec![value, id(proposal.event)];
    cells.extend(proposal.task.map(id));
    cells.push(Cell::new(proposal.agent.to_string(), Tone::Muted));
    line("proposed", cells)
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

/// A node's own status; a candidate shows the group its results last left it in, marked stale
/// while one of them no longer stands, and an open test as proving, or abandoned once its job
/// ended.
fn standing(status: NodeStatus, test: Option<OpenTest>, currency: Currency) -> Cell {
    match (status, test) {
        (NodeStatus::Candidate(status), Some(test)) => {
            let (note, tone) = match test.live {
                true => ("proving", Tone::Accent),
                false => ("test abandoned", Tone::Warning),
            };
            Cell::new(format!("{} ({note})", words(status)), tone)
        }
        (NodeStatus::Candidate(candidate), None) => match currency {
            Currency::Current => self::status(candidate),
            Currency::Stale => {
                let group = self::status(candidate);
                Cell::new(format!("{} (stale)", group.text), group.tone)
            }
        },
        (NodeStatus::Inventory(inventory), _) => inventory_status(inventory),
        (NodeStatus::Path(path), _) => path_status(path),
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

fn inventory_status(status: InventoryStatus) -> Cell {
    let tone = match status {
        InventoryStatus::Recorded => Tone::Normal,
        InventoryStatus::Retracted => Tone::Muted,
    };
    Cell::new(words(status), tone)
}

fn path_status(status: PathStatus) -> Cell {
    let tone = match status {
        PathStatus::Swept => Tone::Success,
        PathStatus::Unswept => Tone::Accent,
        PathStatus::Retracted => Tone::Muted,
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
        FlawStatus::Confirmed | FlawStatus::Refuted => Tone::Muted,
    };
    Cell::new(words(status), tone)
}

#[cfg(test)]
mod tests {
    use skyhook::product::page::Toggle;

    use super::*;
    use crate::store::{
        Begin, Edge, Listed, PremiseScope, Verdict,
        tests::{
            HIGH, assess, begun, component, fixture, job_state as set_job, path, premise, scenario,
            sweep, text, verdict,
        },
    };

    fn section(blocks: &[Block], shelf: ShelfKind) -> &Section {
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

    /// Sections are read only while open: an open section lists its most relevant records first,
    /// a validated candidate before an unassessed one, a stale result in its group marked stale,
    /// with detail for the record opened, marking a result that no longer stands, and a
    /// closed one only its total. A candidate's second column is its score in bold, coloured by
    /// its rating; an unassessed one shows none.
    #[test]
    fn open_sections_list_records_in_order_with_detail_for_open_records() {
        let (_session, store, actor) = fixture();
        let [first, second] = ["decoder", "parser"].map(|name| scenario(&store, &actor, name));
        assess(&store, second.candidate, HIGH);
        verdict(&store, second.candidate, Verdict::Validated);
        let rests = Edge::FlawPremise {
            flaw: second.flaw,
            premise: premise(&store, &actor, PremiseScope::Code),
        };
        store.relink(&actor, &[rests], &[]).unwrap();
        let candidates = ShelfKind::Nodes(NodeKind::Candidate);
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
        assert_eq!(keys[0], [second_key.as_str(), first_key.as_str()]);
        assert!(keys[1..].iter().all(Vec::is_empty));
        let section_of_candidates = section(&blocks, candidates);
        assert_eq!(section_of_candidates.columns[1].title, "cvss");
        let records = &section_of_candidates.records;
        let scores: Vec<&Cell> = records.iter().map(|record| &record.cells[1]).collect();
        assert_eq!(scores, [&Cell::new("9.3", Tone::Error).strong(), &dash()]);
        assert!(records[1].detail.is_none());
        let status = |record: &Record| record.cells[2].text.clone();
        assert_eq!(
            (status(&records[0]), status(&records[1])),
            ("validated (stale)".to_owned(), "identified".to_owned()),
            "a stale result keeps its group, marked stale; one never assessed is identified"
        );
        let detail = records[0].detail.as_ref().unwrap();
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
            "a premise linked since stales the review's verdict, not its assessment"
        );
        let flaws = section(&blocks, ShelfKind::Nodes(NodeKind::Flaw));
        assert_eq!(flaws.total, 2);
    }

    /// Every section's records show one cell per column, a component's and a state's title
    /// under its title, and the coverage panel counts undecided premises of both scopes.
    #[test]
    fn records_fill_their_sections_columns() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        (store.report(&actor, scenario.flaw, &text("overflow"), &text("report"))).unwrap();
        let sweep = Begin::Sweep(Listed::of([scenario.path]));
        let (_, ended, _) = begun(&store, &sweep);
        set_job(&store, ended, JobState::Completed);
        begun(&store, &sweep);
        for scope in [PremiseScope::Code, PremiseScope::Deployment] {
            premise(&store, &actor, scope);
        }
        let mut open = Disclosed::default();
        for shelf in SHELVES {
            open.apply(&Toggle::Section(shelf.key()));
        }
        let blocks = opened(&store, &open);
        for shelf in SHELVES {
            let section = section(&blocks, shelf);
            let widths: Vec<usize> = (section.records.iter())
                .map(|record| record.cells.len())
                .collect();
            assert!(
                !widths.is_empty() && widths.iter().all(|width| *width == section.columns.len()),
                "{}: {widths:?}",
                shelf.title()
            );
        }
        for kind in [NodeKind::Component, NodeKind::State] {
            let section = section(&blocks, ShelfKind::Nodes(kind));
            let title = (section.columns.iter())
                .position(|column| column.title == "title")
                .unwrap();
            assert_eq!(section.records[0].cells[title].text, "decoder");
        }
        let page = (store.run_state().unwrap())
            .page(&store, &Disclosed::default(), None)
            .unwrap();
        let undecided = (page.blocks.iter())
            .find_map(|block| match block {
                Block::Panels(panels) => (panels.iter())
                    .flat_map(|panel| &panel.rows)
                    .find(|row| row.label == "premises undecided"),
                _ => None,
            })
            .unwrap();
        assert_eq!(undecided.values[0].text, "2");
    }

    /// Paths list unswept first, each with the work it owes and its latest task.
    #[test]
    fn paths_list_unswept_first_with_their_work() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let [first, second, third] =
            ["stream", "file", "pipe"].map(|name| path(&store, &actor, name, &[decoder]));
        sweep(&store, first);
        let (_, _, task) = begun(&store, &Begin::Sweep(Listed::of([second])));
        let paths = ShelfKind::Nodes(NodeKind::Path);
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

    /// Live and ended tasks list apart, each newest first, and an open one shows its subjects
    /// in full, its brief, its reply and what its child recorded.
    #[test]
    fn running_and_ended_tasks_list_apart_newest_first() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let stream = path(&store, &actor, "stream", &[decoder]);
        let work = Begin::Sweep(Listed::of([stream]));
        let (_, job, failed) = begun(&store, &work);
        set_job(&store, job, JobState::Failed);
        let (_, job, cancelled) = begun(&store, &work);
        set_job(&store, job, JobState::Cancelled);
        let (_, _, running) = begun(&store, &work);
        let (live, ended) = (
            ShelfKind::Tasks(Liveness::Live),
            ShelfKind::Tasks(Liveness::Ended),
        );
        let mut open = Disclosed::default();
        open.apply(&Toggle::Section(live.key()));
        open.apply(&Toggle::Section(ended.key()));
        open.apply(&Toggle::Record(ended.key(), RecordKey(failed.to_string())));
        let blocks = opened(&store, &open);
        let keys = |shelf| -> Vec<String> {
            (section(&blocks, shelf).records.iter())
                .map(|record| record.key.0.clone())
                .collect()
        };
        assert_eq!(keys(live), [running.to_string()]);
        assert_eq!(
            keys(ended),
            [cancelled, failed].map(|task| task.to_string())
        );
        let detail = section(&blocks, ended).records[1].detail.as_ref().unwrap();
        let labels: Vec<_> = (detail.lines.iter())
            .filter_map(|line| line.label.as_deref())
            .collect();
        assert_eq!(labels, ["subjects", "brief", "reply", "records"]);
    }
}
