//! The run as the Research tab sees it.
use std::fmt::Display;

use skyhook::product::page::{
    Block, Cell, Disclosed, Fact, Meter, Page, Panel, PanelRow, Stage, StageState, Tone,
};

use crate::store::{FlawStatus, Phase, PremiseStatus, RunState, Status, Store, Summary};

mod sections;
use sections::sections;

/// When this process opened the session; a reopen starts it again.
pub(crate) struct Opened {
    at: chrono::DateTime<chrono::Local>,
    since: std::time::Instant,
}

impl Opened {
    pub(crate) fn now() -> Self {
        Self {
            at: chrono::Local::now(),
            since: std::time::Instant::now(),
        }
    }

    /// How long the session has been open, to the whole minute: the page is read again
    /// only as often as that changes.
    pub(crate) fn uptime(&self) -> Uptime {
        Uptime {
            at: self.at,
            minutes: self.since.elapsed().as_secs() / 60,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Uptime {
    at: chrono::DateTime<chrono::Local>,
    minutes: u64,
}

impl Display for Uptime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (hours, minutes) = (self.minutes / 60, self.minutes % 60);
        write!(f, "{} (", self.at.format("%H:%M"))?;
        match hours {
            0 => write!(f, "{minutes}m)"),
            _ => write!(f, "{hours}h {minutes:02}m)"),
        }
    }
}

pub(crate) const VIEWS: &[&str] = &["Research"];

impl RunState {
    /// The Research tab: the run, how long it has been open when it is, where its plan
    /// stands, where each phase stands and counts that use the width, then the record
    /// sections with what `disclosed` shows.
    pub(crate) fn page(
        &self,
        store: &Store,
        disclosed: &Disclosed,
        uptime: Option<Uptime>,
    ) -> Result<Page, crate::store::Error> {
        let facts = &self.facts;
        let summary = &facts.summary;
        let coverage = &self.coverage;
        let mut header = vec![
            fact("run", facts.run.id.to_string()),
            fact("threshold", facts.run.options.threshold.to_string()),
            fact("live", facts.run.options.live.to_string()),
            fact("workers", facts.run.options.workers.to_string()),
        ];
        header.extend(uptime.map(|uptime| fact("opened", uptime.to_string())));
        let plan = Fact {
            label: "plan".to_owned(),
            value: Cell::new(
                summary.line(),
                if summary.complete() {
                    Tone::Success
                } else if StageWork::of(summary, &Phase::ALL).active {
                    Tone::Accent
                } else {
                    Tone::Warning
                },
            ),
        };
        let work = |phases: &[Phase]| StageWork::of(summary, phases);
        let began = |phases: &[Phase]| {
            (phases.iter()).any(|phase| coverage.tasks.get(phase).is_some_and(|count| *count > 0))
        };
        let questions = [Phase::Settle, Phase::Chain];
        let structure = [Phase::Sweep, Phase::Probe, Phase::Check];
        let owed =
            |phases: &[Phase]| -> u64 { phases.iter().map(|phase| summary.owed(*phase)).sum() };
        let mut test = work(&[Phase::Test]);
        test.exhausted |= self.open_tests.iter().any(|open| !open.live);
        let pipeline = vec![
            stage(
                "target",
                coverage.components,
                StageWork::default(),
                coverage.components > 0,
            ),
            stage(
                "paths",
                coverage.paths,
                StageWork {
                    active: coverage.states > 0 && coverage.paths == 0,
                    exhausted: false,
                },
                coverage.paths > 0,
            ),
            stage(
                "sweep",
                format!("{}/{}", coverage.swept, coverage.paths),
                work(&structure),
                coverage.swept > 0,
            ),
            stage(
                "questions",
                owed(&questions),
                work(&questions),
                began(&questions),
            ),
            stage(
                "review",
                summary.owed(Phase::Review),
                work(&[Phase::Review]),
                began(&[Phase::Review]),
            ),
            stage(
                "test",
                summary.owed(Phase::Test),
                test,
                began(&[Phase::Test]),
            ),
            stage(
                "report",
                coverage.findings,
                work(&[Phase::Report]),
                coverage.findings > 0,
            ),
        ];
        let count = |value: u64| Cell::from(value.to_string());
        let flagged = |value: u64| {
            Cell::new(
                value.to_string(),
                if value > 0 {
                    Tone::Warning
                } else {
                    Tone::Muted
                },
            )
        };
        let premises =
            |status: PremiseStatus| coverage.premises.get(&status).copied().unwrap_or_default();
        let flaws = |status: FlawStatus| coverage.flaws.get(&status).copied().unwrap_or_default();
        let coverage_panel = Panel {
            title: "Coverage".to_owned(),
            columns: Vec::new(),
            rows: vec![
                row("states", count(coverage.states), None),
                row("paths", count(coverage.paths), None),
                row(
                    "  swept",
                    count(coverage.swept),
                    Some(Meter {
                        value: coverage.swept,
                        total: coverage.paths,
                    }),
                ),
                row("  unswept", flagged(coverage.paths - coverage.swept), None),
                row(
                    "  exhausted",
                    flagged(
                        structure
                            .iter()
                            .map(|phase| summary.exhausted(*phase))
                            .sum(),
                    ),
                    None,
                ),
                row("  uncovered interfaces", flagged(summary.uncovered), None),
                row("  outside scope", count(coverage.outside.paths), None),
                row("components", count(coverage.components), None),
                row(
                    "  reached",
                    count(coverage.components - coverage.unreached),
                    None,
                ),
                row("  mapped only", flagged(coverage.unreached), None),
                row(
                    "premises undecided",
                    count(premises(PremiseStatus::Undecided)),
                    None,
                ),
                row("  asked", flagged(summary.asked), None),
                row("  disputed", flagged(coverage.disputed), None),
                row("flaws suspected", count(flaws(FlawStatus::Suspected)), None),
                row("  confirmed", count(flaws(FlawStatus::Confirmed)), None),
                row("  refuted", count(flaws(FlawStatus::Refuted)), None),
            ],
        };
        let candidates = |status: Status| {
            coverage
                .candidates
                .get(&status)
                .copied()
                .unwrap_or_default()
        };
        let shown = || (Status::ALL.into_iter()).filter(|status| *status != Status::Merged);
        // The funnel compares statuses with each other, so the largest fills its bar.
        let largest = shown().map(candidates).max().unwrap_or_default();
        let mut rows: Vec<PanelRow> = shown()
            .map(|status| {
                let value = candidates(status);
                row(
                    &words(status),
                    count(value),
                    Some(Meter {
                        value,
                        total: largest,
                    }),
                )
            })
            .collect();
        let abandoned = self.open_tests.iter().filter(|open| !open.live).count() as u64;
        rows.push(row(
            "proving",
            count(self.open_tests.len() as u64 - abandoned),
            None,
        ));
        if abandoned > 0 {
            rows.push(row("test abandoned", flagged(abandoned), None));
        }
        if coverage.outside.candidates > 0 {
            rows.push(row(
                "outside scope",
                count(coverage.outside.candidates),
                None,
            ));
        }
        let funnel = Panel {
            title: "Candidates".to_owned(),
            columns: Vec::new(),
            rows,
        };
        let mut blocks = vec![
            Block::Facts(header),
            Block::Facts(vec![plan]),
            Block::Pipeline(pipeline),
            Block::Panels(vec![coverage_panel, funnel, tasks_panel(summary)]),
        ];
        blocks.extend(sections(store, self, disclosed)?);
        Ok(Page { blocks })
    }
}

/// Each task phase's running tasks and exhausted subjects.
fn tasks_panel(summary: &Summary) -> Panel {
    Panel {
        title: "Tasks".to_owned(),
        columns: vec!["running".to_owned(), "exhausted".to_owned()],
        rows: (Phase::ALL.into_iter())
            .map(|phase| {
                let cell = |count: u64, tone| match count {
                    0 => Cell::new("0", Tone::Muted),
                    count => Cell::new(count.to_string(), tone),
                };
                PanelRow {
                    label: words(phase),
                    values: vec![
                        cell(summary.running(phase), Tone::Accent),
                        cell(summary.exhausted(phase), Tone::Warning),
                    ],
                    meter: None,
                }
            })
            .collect(),
    }
}

fn words(value: impl Display) -> String {
    value.to_string().replace('_', " ")
}

fn fact(label: &str, value: String) -> Fact {
    Fact {
        label: label.to_owned(),
        value: value.into(),
    }
}

fn row(label: &str, value: Cell, meter: Option<Meter>) -> PanelRow {
    PanelRow {
        label: label.to_owned(),
        values: vec![value],
        meter,
    }
}

/// Where a stage's phases stand: at work while any of it is owed or running, blocked when
/// some of it is exhausted (for tests, also while one waits on the user).
#[derive(Clone, Copy, Default)]
struct StageWork {
    active: bool,
    exhausted: bool,
}

impl StageWork {
    fn of(summary: &Summary, phases: &[Phase]) -> Self {
        Self {
            active: (phases.iter()).any(|phase| summary.owed(*phase) + summary.running(*phase) > 0),
            exhausted: (phases.iter()).any(|phase| summary.exhausted(*phase) > 0),
        }
    }

    /// At work, else blocked, done once it has started, or not reached yet.
    fn state(self, started: bool) -> StageState {
        if self.active {
            StageState::Active
        } else if self.exhausted {
            StageState::Blocked
        } else if started {
            StageState::Done
        } else {
            StageState::Pending
        }
    }
}

fn stage(label: &str, value: impl Display, work: StageWork, started: bool) -> Stage {
    Stage {
        label: label.to_owned(),
        value: value.to_string(),
        state: work.state(started),
    }
}
