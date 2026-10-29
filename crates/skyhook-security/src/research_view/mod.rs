//! The run as the orchestrator and the Research tab see it: one read, two renderings.
use std::fmt::Display;

use skyhook::identity::JobId;

use skyhook_cli::{
    ExtensionView,
    page::{
        AgentPane, Block, Cell, Disclosed, Fact, Meter, Page, Panel, PanelRow, Stage, StageState,
        Tone, ViewFrame,
    },
};

use crate::{
    session::drift_warning,
    store::{
        ComponentCoverage, ContractState, OpenTest, Outcome, PathCoverage, Phase, PlanStanding,
        Revision, RunFacts, RunState, Status, Store, Synthesis, UnitKind,
    },
};

mod sections;
use sections::sections;

pub const VIEWS: &[ExtensionView] = &[ExtensionView {
    title: "Research",
    frame: ViewFrame::Page {
        agents: AgentPane::Live,
    },
}];

impl RunFacts {
    fn drifted(&self, current: &Revision) -> bool {
        self.run.revision != *current
    }

    /// The orchestrator's section of its runtime state, with `current` the checkout as
    /// last read and `waiting` the live workflows whose latest scheduling page held tests
    /// with nothing to start.
    pub fn state(&self, current: &Revision) -> String {
        self.state_with(current, &[])
    }

    pub fn state_with(&self, current: &Revision, waiting: &[JobId]) -> String {
        let drift = match self.drifted(current) {
            true => format!(
                "drift: recorded {}; recheck affected evidence\n",
                self.run.revision
            ),
            false => String::new(),
        };
        let owed: Vec<String> = (self.synthesis.findings.iter())
            .filter(|(_, count)| **count > 0)
            .map(|(phase, count)| format!("{phase}={count}"))
            .collect();
        let owed = match owed.is_empty() {
            true => "none".to_owned(),
            false => owed.join(" "),
        };
        let attention: String = waiting
            .iter()
            .map(|script| {
                format!(
                    "Workflow script {script} has no active child agents; its latest startable \
                     page offered no work and held tests for approval, an unblock or a slot. Check \
                     pending PoC approvals, pending tests and test slots, and read the latest \
                     scope's asked conditions. Handle any outstanding user decisions before \
                     waiting for the script to finish; polling alone will not supply them. Never \
                     infer approval from this reminder.\n"
                )
            })
            .collect();
        format!(
            "run: {}\nrevision: {current}\n{drift}poc: {}\nworkers: {}\nplan: {}\nowed: {owed}\nartifacts: {}\n{attention}",
            self.run.id,
            self.run.poc,
            self.run.options.workers,
            self.synthesis.line(),
            self.artifacts.display(),
        )
    }
}

impl RunState {
    /// The Research tab: the run, where its plan stands, where each phase stands and
    /// counts that use the width, then the record sections with what `disclosed` shows.
    pub fn page(
        &self,
        store: &Store,
        current: &Revision,
        disclosed: &Disclosed,
        tests: &[OpenTest],
    ) -> Result<Page, crate::store::Error> {
        let coverage = &store.coverage()?;
        let facts = &self.facts;
        let synthesis = &facts.synthesis;
        let mut header = vec![
            fact("run", facts.run.id.to_string()),
            fact("revision", current.to_string()),
            fact("threshold", facts.run.options.threshold.to_string()),
            fact("poc", facts.run.poc.to_string()),
            fact("workers", facts.run.options.workers.to_string()),
        ];
        if facts.drifted(current) {
            let warning = drift_warning(&facts.run.revision, current);
            header.push(Fact {
                label: "drift".to_owned(),
                value: Cell::new(warning, Tone::Warning),
            });
        }
        let open = |kind| synthesis.tally(kind).open;
        let plan = Fact {
            label: "plan".to_owned(),
            value: Cell::new(
                synthesis.line(),
                match &synthesis.standing {
                    PlanStanding::NotPlanned => Tone::Muted,
                    PlanStanding::InProgress => Tone::Accent,
                    _ if synthesis.complete() && synthesis.limited() == 0 => Tone::Success,
                    PlanStanding::Ended { .. } => Tone::Warning,
                },
            ),
        };
        let units = |kind| synthesis.tally(kind).total();
        let done = |kind| synthesis.tally(kind).ended(Outcome::Completed);
        let blocked = |kind| synthesis.tally(kind).ended(Outcome::Blocked);
        let questions = [UnitKind::Question, UnitKind::FollowUp].map(|kind| synthesis.tally(kind));
        let open_questions = questions.iter().map(|tally| tally.open).sum::<u64>();
        let blocked_questions = questions
            .iter()
            .map(|tally| tally.ended(Outcome::Blocked))
            .sum::<u64>();
        let finding = |phases: &[Phase]| {
            phases
                .iter()
                .map(|phase| synthesis.owed(*phase))
                .sum::<u64>()
        };
        let progress = &self.progress;
        let pipeline = vec![
            stage(
                "target",
                progress.components,
                0,
                false,
                progress.components > 0,
            ),
            stage(
                "paths",
                progress.paths,
                u64::from(progress.sources > 0 && progress.paths == 0),
                false,
                progress.paths > 0,
            ),
            stage(
                "sweep",
                format!("{}/{}", done(UnitKind::Sweep), units(UnitKind::Sweep)),
                open(UnitKind::Sweep),
                blocked(UnitKind::Sweep) > 0,
                units(UnitKind::Sweep) > 0,
            ),
            stage(
                "path review",
                format!(
                    "{}/{}",
                    coverage.paths_in(PathCoverage::Checked),
                    progress.paths
                ),
                open(UnitKind::PathReview) + synthesis.owed(Phase::Assess),
                blocked(UnitKind::PathReview) > 0,
                units(UnitKind::PathReview) > 0,
            ),
            stage(
                "questions",
                open_questions,
                open_questions,
                blocked_questions > 0,
                units(UnitKind::Question) + units(UnitKind::FollowUp) > 0,
            ),
            stage(
                "review",
                synthesis.owed(Phase::Review),
                synthesis.owed(Phase::Review),
                false,
                self.reviewed,
            ),
            stage(
                "poc",
                finding(&[Phase::Poc, Phase::PocPending]),
                synthesis.owed(Phase::Poc),
                synthesis.owed(Phase::PocPending) > 0,
                self.tested,
            ),
            stage(
                "report",
                self.findings,
                synthesis.owed(Phase::Report),
                false,
                self.findings > 0,
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
        let inspected = coverage.components_in(ComponentCoverage::Inspected);
        let checked = coverage.paths_in(PathCoverage::Checked);
        let coverage_panel = Panel {
            title: "Coverage".to_owned(),
            columns: Vec::new(),
            rows: vec![
                row("sources", count(self.progress.sources), None),
                row("paths", count(self.progress.paths), None),
                row(
                    "  checked",
                    count(checked),
                    Some(Meter {
                        value: checked,
                        total: self.progress.paths,
                    }),
                ),
                row(
                    "  unchecked",
                    flagged(coverage.paths_in(PathCoverage::Unchecked)),
                    None,
                ),
                row(
                    "  unreached",
                    flagged(coverage.paths_in(PathCoverage::Unreached)),
                    None,
                ),
                row("components", count(self.progress.components), None),
                row(
                    "  inspected",
                    count(inspected),
                    Some(Meter {
                        value: inspected,
                        total: self.progress.components,
                    }),
                ),
                row(
                    "  reached",
                    count(coverage.components_in(ComponentCoverage::Reached)),
                    None,
                ),
                row(
                    "  mapped only",
                    count(coverage.components_in(ComponentCoverage::Mapped)),
                    None,
                ),
                row("deferred", flagged(coverage.deferred), None),
                row(
                    "contracts",
                    (ContractState::ALL.iter())
                        .map(|state| format!("{} {state}", coverage.contracts_in(*state)))
                        .collect::<Vec<_>>()
                        .join(" · ")
                        .into(),
                    None,
                ),
                row("unchallenged", flagged(coverage.unchallenged), None),
                row("open flaws", count(self.progress.flaws), None),
            ],
        };
        let shown = || (Status::ALL.into_iter()).filter(|status| *status != Status::Merged);
        // The funnel compares statuses with each other, so the largest fills its bar.
        let largest = (shown().map(|status| self.progress.candidates_in(status)))
            .max()
            .unwrap_or_default();
        let mut rows: Vec<PanelRow> = shown()
            .map(|status| {
                let value = self.progress.candidates_in(status);
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
        // Open tests are shown as read now; they change what the candidates show, not owe.
        let (proving, abandoned) =
            (tests.iter()).fold((0, 0), |(live, dead), test| match test.live {
                true => (live + 1, dead),
                false => (live, dead + 1),
            });
        rows.push(row("proving", count(proving), None));
        if abandoned > 0 {
            rows.push(row("test abandoned", flagged(abandoned), None));
        }
        let funnel = Panel {
            title: "Candidates".to_owned(),
            columns: Vec::new(),
            rows,
        };
        let plan_panel = plan_panel(synthesis);
        let mut blocks = vec![
            Block::Facts(header),
            Block::Facts(vec![plan]),
            Block::Pipeline(pipeline),
            Block::Panels(vec![coverage_panel, funnel, plan_panel]),
        ];
        blocks.extend(sections(store, self, coverage, disclosed, tests)?);
        Ok(Page { blocks })
    }
}

/// Plan units of each kind: open, completed, and ended with a limitation (blocked,
/// inconclusive or deferred, which the plan line counts apart).
fn plan_panel(synthesis: &Synthesis) -> Panel {
    Panel {
        title: "Plan".to_owned(),
        columns: vec!["open".to_owned(), "done".to_owned(), "limited".to_owned()],
        rows: UnitKind::ALL
            .iter()
            .map(|kind| {
                let cell = |count: u64, tone| match count {
                    0 => Cell::new("0", Tone::Muted),
                    count => Cell::new(count.to_string(), tone),
                };
                let tally = synthesis.tally(*kind);
                PanelRow {
                    label: words(kind),
                    values: vec![
                        cell(tally.open, Tone::Accent),
                        cell(tally.ended(Outcome::Completed), Tone::Normal),
                        cell(tally.limited(), Tone::Warning),
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

/// At work while anything is owed, else blocked when some of it ended blocked, done once
/// it has started, or not reached yet.
fn state(owed: u64, blocked: bool, started: bool) -> StageState {
    if owed > 0 {
        StageState::Active
    } else if blocked {
        StageState::Blocked
    } else if started {
        StageState::Done
    } else {
        StageState::Pending
    }
}

fn stage(label: &str, value: impl Display, owed: u64, blocked: bool, started: bool) -> Stage {
    Stage {
        label: label.to_owned(),
        value: value.to_string(),
        state: state(owed, blocked, started),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{
        PocResult,
        tests::{fixture, poc, report_input, revision, root_of, validated},
    };

    /// Review and PoC read as done once their work ran, whatever the candidates moved on
    /// A held workflow is named in the root's state only while it waits, and the rest of the
    /// state is unchanged by it.
    #[test]
    fn waiting_workflow_attention_is_appended_to_the_root_state() {
        let (_session, store, actor) = fixture();
        let facts = store.run_facts().unwrap();
        let current = revision();
        let plain = facts.state(&current);
        assert!(!plain.contains("Workflow script"));
        assert_eq!(facts.state_with(&current, &[]), plain);

        let workflow = skyhook::identity::JobId::new(4242).unwrap();
        let waiting = facts.state_with(&current, &[workflow]);
        assert!(waiting.starts_with(&plain), "{waiting}");
        let attention = &waiting[plain.len()..];
        assert!(
            attention.starts_with("Workflow script 4242 has no active child agents"),
            "{attention}"
        );
        assert!(attention.contains("pending PoC approvals"), "{attention}");
        assert!(attention.ends_with('\n'));
        let _ = (store, actor);
    }

    /// to: here a failed test of a scenario since reported.
    #[test]
    fn stages_are_done_once_their_work_ran() {
        let (_session, store, actor) = fixture();
        let id = validated(&store, &actor);
        poc(&store, &actor, id, PocResult::Failed);
        let root = root_of(&store, id);
        let version = store.report_generation(root).unwrap().key;
        store
            .record_finding(&actor, &report_input(root), &version, &[])
            .unwrap();
        let state = store.run_state().unwrap();
        let page = (state.page(&store, &revision(), &Disclosed::default(), &[])).unwrap();
        let stages = (page.blocks.iter())
            .find_map(|block| match block {
                Block::Pipeline(stages) => Some(stages),
                _ => None,
            })
            .expect("a pipeline");
        let stage = |label: &str| {
            (stages.iter())
                .find(|stage| stage.label == label)
                .unwrap_or_else(|| panic!("a {label} stage"))
                .state
        };
        assert_eq!(
            ["review", "poc", "report"].map(stage),
            [StageState::Done; 3]
        );
    }
}
