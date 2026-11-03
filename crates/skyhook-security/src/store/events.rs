//! The audit log: every write is one event naming the nodes it bears on, each with the
//! strongest effect it has on it. A write names its direct marks; one rule carries them
//! along the graph: a premise moves the standing of the flaws and paths relying on it, a state
//! passes its effect to the paths accessing it, and a flaw or path passes its effect to the live
//! candidates holding it. Some marks move their node's own work alone: a candidate taking or
//! losing a flaw as its root leaves the flaw's other candidates unmoved, an access recorded on a
//! state, or its path retracted, restored or merged away, moves its check alone, and a status set
//! on a path, state or component moves that node, and a state's paths, but no candidate, unless
//! it retracts or restores a path. A flaw joining or leaving a chain at another place is only
//! concerned.

use std::collections::{BTreeMap, HashMap};

use libsql::Row;
use schemars::JsonSchema;
use serde::Serialize;
use skyhook::{
    db::{Db, Sql, enum_column, params},
    identity::SessionId,
};

use super::*;

/// How one write bears on each node it names, the strongest effect kept: marks the rule
/// carries along the graph, and marks that stay on their node.
#[derive(Default)]
pub(super) struct Marks {
    carried: BTreeMap<NodeId, EventEffect>,
    alone: BTreeMap<NodeId, EventEffect>,
}

fn strongest(marks: &mut BTreeMap<NodeId, EventEffect>, node: NodeId, effect: EventEffect) {
    let held = marks.entry(node).or_insert(effect);
    *held = (*held).max(effect);
}

impl Marks {
    pub(super) fn of(node: impl Into<NodeId>, effect: EventEffect) -> Self {
        let mut marks = Self::default();
        marks.mark(node, effect);
        marks
    }

    pub(super) fn mark(&mut self, node: impl Into<NodeId>, effect: EventEffect) {
        strongest(&mut self.carried, node.into(), effect);
    }

    /// Mark `node` standing for its own work alone, carried nowhere.
    pub(super) fn alone(&mut self, node: impl Into<NodeId>) {
        strongest(&mut self.alone, node.into(), EventEffect::Standing);
    }

    /// Mark alone the check of each state `path` accesses, as recording or removing those
    /// accesses would: a path retracted, restored or merged away changes them.
    pub(super) fn accesses(&mut self, db: &Db, path: PathId) -> Result<()> {
        let sql = "SELECT DISTINCT state FROM path_state WHERE path=?1";
        for state in rows(db, sql, params![path], |row| Ok(NodeId(row.get(0)?)))? {
            self.alone(state);
        }
        Ok(())
    }

    /// Mark alone the flaws leaving and taking a chain's root role, when it moved.
    pub(super) fn rerooted(&mut self, before: Option<FlawId>, after: Option<FlawId>) {
        if before != after {
            for flaw in [before, after].into_iter().flatten() {
                self.alone(flaw);
            }
        }
    }

    /// The marks that move what a node's work rests on, as `[[node, effect], ...]`.
    fn moving(&self) -> Result<String> {
        let moving: Vec<(i64, EventEffect)> = (self.carried.iter())
            .filter(|(_, effect)| **effect >= EventEffect::Standing)
            .map(|(node, effect)| (node.0, *effect))
            .collect();
        json_list(&moving)
    }

    /// Mark each `(node, effect)` row `sql` selects from the moving marks bound as `?1`.
    fn carry(&mut self, db: &Db, sql: &'static str) -> Result<()> {
        let carried = rows(db, sql, params![self.moving()?], |row| {
            Ok((NodeId(row.get(0)?), enum_column::<EventEffect>(row, 1)?))
        })?;
        for (node, effect) in carried {
            self.mark(node, effect);
        }
        Ok(())
    }

    /// Every mark, with what the carried ones carry along the graph as it stands after the
    /// write.
    fn propagated(mut self, db: &Db) -> Result<BTreeMap<NodeId, EventEffect>> {
        self.carry(
            db,
            "SELECT fp.flaw,'standing' FROM json_each(?1) j JOIN flaw_premise fp ON \
             fp.premise=j.value->>0 UNION SELECT pp.path,'standing' FROM json_each(?1) j JOIN \
             path_premise pp ON pp.premise=j.value->>0 UNION SELECT ps.path,j.value->>1 FROM \
             json_each(?1) j JOIN path_state ps ON ps.state=j.value->>0",
        )?;
        self.carry(
            db,
            "SELECT cf.candidate,j.value->>1 FROM json_each(?1) j JOIN candidate_flaw cf ON \
             cf.flaw=j.value->>0 JOIN node n ON n.id=cf.candidate WHERE n.merged_into IS NULL \
             UNION ALL SELECT ca.candidate,j.value->>1 FROM json_each(?1) j JOIN candidate_action \
             ca ON ca.path=j.value->>0 JOIN node n ON n.id=ca.candidate WHERE n.merged_into IS NULL",
        )?;
        for (node, effect) in self.alone {
            strongest(&mut self.carried, node, effect);
        }
        Ok(self.carried)
    }
}

/// A phase's results on a node: a premise's status, a flaw's judgement, a candidate's assessment
/// or verdict, each recorded with whether the work owning it recorded it.
pub(super) struct Standing {
    /// Whether event `?2` was recorded by the work owning node `?1`'s results.
    pub(super) owns: &'static str,
    /// Whether node `?1`'s standing result is `?2`.
    pub(super) held: &'static str,
    /// Whether node `?1`'s proposal is event `?2`.
    pub(super) proposal: &'static str,
    /// The effect of a result its owning work records that replaces the standing one.
    pub(super) replaced: EventEffect,
}

/// The [`Standing`] of the results in `$view`, keyed by `$node`, whose value is `$value` and
/// whose proposal is `$proposed`, owned by the work `$owns` holds for.
macro_rules! standing_of {
    ($view:literal, $node:literal, $value:literal, $proposed:literal, $owns:expr,
        $replaced:ident) => {
        Standing {
            owns: concat!("SELECT ", $owns),
            held: concat!(
                "SELECT ",
                $value,
                " IS ?2 FROM ",
                $view,
                " WHERE ",
                $node,
                "=?1"
            ),
            proposal: concat!(
                "SELECT ",
                $proposed,
                " IS ?2 FROM ",
                $view,
                " WHERE ",
                $node,
                "=?1"
            ),
            replaced: EventEffect::$replaced,
        }
    };
}
pub(super) use standing_of;

impl Standing {
    /// The effect on `node` of its result `value`, which `insert` records at `event` from `row`
    /// and whether its owning work recorded it, bound last. Recorded by the work owning it, it
    /// replaces the standing one, or only concerns it when it restates it, current or not: its
    /// currency comes from that work's basis, and nothing resting on it moves. Recorded
    /// elsewhere, it disputes the standing one when it reads as its proposal, moving that work
    /// alone, and only concerns it when it agrees or repeats a proposal that work answered.
    pub(super) fn record(
        &self,
        db: &Db,
        node: impl Into<NodeId>,
        event: EventId,
        value: impl Sql,
        insert: &'static str,
        mut row: Vec<libsql::Value>,
    ) -> Result<EventEffect> {
        let node = node.into();
        let flag =
            |sql, value| stored(db, sql, params![node, value], |row| Ok(row.get::<bool>(0)?));
        let owned = flag(self.owns, event.sql())?;
        let replaces = owned && !flag(self.held, value.sql())?;
        row.push(owned.sql());
        db.execute(insert, row)?;
        Ok(if replaces {
            self.replaced
        } else if !owned && flag(self.proposal, event.sql())? {
            EventEffect::Disputes
        } else {
            EventEffect::Concerns
        })
    }
}

/// Record one event of `kind` by `actor`, part of the tasks of the outermost agent holding any,
/// `actor` or one above it: `write` makes its changes knowing the event, and names the nodes it
/// bears on directly; the event then names them and what they carry, and the bearing they decide.
pub(super) fn audit<T>(
    db: &Db,
    actor: &AgentId,
    kind: EventKind,
    note: Option<&Text>,
    write: impl FnOnce(EventId) -> Result<(T, Marks)>,
) -> Result<T> {
    let agent = agent_row(db, actor)?;
    let event = EventId(db.insert(
        "INSERT INTO event(agent,at,kind,note) VALUES (?1,?2,?3,?4)",
        params![agent, now(), kind, note],
    )?);
    db.execute(
        "WITH RECURSIVE line(agent,depth) AS (SELECT ?2,0 UNION ALL SELECT g.parent,l.depth+1 \
         FROM line l JOIN agent g ON g.id=l.agent WHERE g.parent IS NOT NULL),held AS (SELECT \
         t.id,l.depth FROM line l JOIN agent a ON a.id=l.agent JOIN task t ON \
         t.job=a.owner_job) INSERT INTO event_task(event,task) SELECT ?1,id FROM held WHERE \
         depth=(SELECT max(depth) FROM held)",
        params![event, agent],
    )?;
    bearing::record_work(db, event, kind)?;
    let (written, marks) = write(event)?;
    let marks = marks.propagated(db)?;
    for (node, effect) in &marks {
        db.execute(
            "INSERT INTO event_node(event,node,kind,effect) VALUES (?1,?2,?3,?4)",
            params![event, *node, kind, *effect],
        )?;
    }
    bearing::record(db, event, kind, &marks)?;
    Ok(written)
}

/// A node an event names and how it bears on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Mark {
    pub node: NodeId,
    pub effect: EventEffect,
}

/// `note` is the actor's own words.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Event {
    pub id: EventId,
    pub kind: EventKind,
    pub agent: AgentId,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub nodes: Vec<Mark>,
}

impl Keyed for Event {
    fn key(&self) -> i64 {
        self.id.0
    }
}

/// Events read as `e`, decoded by [`event_row`] without their nodes; a condition follows.
macro_rules! event_select {
    () => {
        "SELECT e.id,e.kind,e.agent,e.at,e.note FROM event e WHERE "
    };
}

/// An event as its row holds it: its agent is a journal row, and its nodes are apart.
struct Logged {
    id: EventId,
    kind: EventKind,
    agent: i64,
    at: DateTime<Utc>,
    note: Option<String>,
}

fn event_row(row: &Row) -> Result<Logged> {
    Ok(Logged {
        id: EventId(row.get(0)?),
        kind: enum_column(row, 1)?,
        agent: row.get(2)?,
        at: time(row.get(3)?)?,
        note: row.get(4)?,
    })
}

/// `events` with their agents and the nodes each names.
fn completed(db: &Db, session: SessionId, events: Vec<Logged>) -> Result<Vec<Event>> {
    let ids: Vec<i64> = events.iter().map(|event| event.id.0).collect();
    let mut nodes = BTreeMap::<i64, Vec<Mark>>::new();
    for (event, mark) in rows(
        db,
        "SELECT en.event,en.node,en.effect FROM json_each(?1) j JOIN event_node en ON \
         en.event=j.value ORDER BY en.event,en.node",
        params![json_list(&ids)?],
        |row| {
            let mark = Mark {
                node: NodeId(row.get(1)?),
                effect: enum_column(row, 2)?,
            };
            Ok((row.get::<i64>(0)?, mark))
        },
    )? {
        nodes.entry(event).or_default().push(mark);
    }
    let mut agents = HashMap::new();
    (events.into_iter())
        .map(|event| {
            let agent = match agents.get(&event.agent) {
                Some(agent) => Clone::clone(agent),
                None => {
                    let agent = agent_id(db, session, event.agent)?;
                    agents.insert(event.agent, agent.clone());
                    agent
                }
            };
            Ok(Event {
                id: event.id,
                kind: event.kind,
                agent,
                at: event.at,
                note: event.note,
                nodes: nodes.remove(&event.id.0).unwrap_or_default(),
            })
        })
        .collect()
}

/// The agent of journal row `row`, by its path of child indexes from the root.
pub(super) fn agent_id(db: &Db, session: SessionId, row: i64) -> Result<AgentId> {
    let path = rows(
        db,
        "WITH RECURSIVE up(id,child,depth) AS (SELECT id,child_index,0 FROM agent WHERE id=?1 \
         UNION ALL SELECT a.id,a.child_index,up.depth+1 FROM up JOIN agent g ON g.id=up.id JOIN \
         agent a ON a.id=g.parent) SELECT child FROM up WHERE child IS NOT NULL ORDER BY depth \
         DESC",
        params![row],
        |row| Ok(row.get::<u32>(0)?),
    )?;
    Ok((path.into_iter()).fold(AgentId::root(session), |agent, index| agent.child(index)))
}

/// The first `limit` events the agent of journal job `job` recorded, oldest first.
pub(super) fn of_job(db: &Db, session: SessionId, job: i64, limit: usize) -> Result<Vec<Event>> {
    let events = first_rows(
        db,
        concat!(
            event_select!(),
            "e.agent=(SELECT id FROM agent WHERE owner_job=?1) ORDER BY e.id"
        ),
        params![job],
        limit,
        event_row,
    )?;
    completed(db, session, events)
}

impl Store {
    /// Every event, or the events naming `node`, oldest first.
    pub fn history(&self, node: Option<NodeId>, page: Page) -> Result<Listing<Event>> {
        let session = self.id();
        self.read(|db| {
            let events = match node {
                None => first_rows(
                    db,
                    concat!(event_select!(), "e.id>?1 ORDER BY e.id"),
                    params![page.after],
                    page.limit(),
                    event_row,
                )?,
                Some(node) => first_rows(
                    db,
                    "SELECT e.id,e.kind,e.agent,e.at,e.note FROM event_node en JOIN event e ON \
                     e.id=en.event WHERE en.node=?2 AND en.event>?1 ORDER BY en.event",
                    params![page.after, node],
                    page.limit(),
                    event_row,
                )?,
            };
            Ok(Listing::keyset(completed(db, session, events)?, page))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            HIGH, access, assess, begun, check, child_agent, component, decide, fixture,
            last_event, location, marks, path, premise, scenario, state, sweep, test, text,
            validated, verdict,
        },
        *,
    };

    /// A write's effects flow premise → flaw/path and state → path → candidate: a flaw edit
    /// makes its candidates `changed`, a premise decision makes its flaws, paths and their
    /// candidates `standing`, a path step or an input state's edit makes the candidates along
    /// the path `changed`, and a test record moves only its own candidate. A candidate taking a
    /// flaw as its root moves that flaw, not its other candidates; a flaw joining or leaving a
    /// chain at another place is only concerned. An access recorded moves its path's standing
    /// and its state's check alone.
    #[test]
    fn effects_propagate_along_the_graph() {
        let (_session, store, actor) = fixture();
        let decoder = validated(&store, &actor, "decoder");
        let (candidate, flaw, path) = (decoder.candidate, decoder.flaw, decoder.path);
        let other = scenario(&store, &actor, "parser").candidate;
        let [on_flaw, on_path] = [(); 2].map(|_| premise(&store, &actor, PremiseScope::Code));
        let links = [
            Link::FlawPremise {
                flaw,
                premise: on_flaw,
            },
            Link::PathPremise {
                path,
                premise: on_path,
            },
        ];
        store.link(&actor, &links).unwrap();
        let concerns = |node: NodeId| (node, EventEffect::Concerns);
        let standing = |node: NodeId| (node, EventEffect::Standing);
        let changed = |node: NodeId| (node, EventEffect::Changed);
        assert_eq!(
            marks(&store, last_event(&store)),
            [
                standing(path.into()),
                standing(flaw.into()),
                standing(candidate.into()),
                concerns(on_flaw.into()),
                concerns(on_path.into()),
            ]
        );

        let edit = Edit {
            title: None,
            body: Some(text("the copy is unbounded on retry")),
            detail: FlawEdit::default(),
        };
        store
            .upsert(&actor, &Upsert::Flaw(Change::Edit(flaw, edit)))
            .unwrap();
        assert_eq!(
            marks(&store, last_event(&store)),
            [changed(flaw.into()), changed(candidate.into())]
        );

        for premise in [on_flaw, on_path] {
            decide(&store, premise, PremiseStatus::Holds);
        }
        assert_eq!(
            marks(&store, last_event(&store)),
            [
                standing(path.into()),
                standing(candidate.into()),
                changed(on_path.into())
            ]
        );

        let component = decoder.component;
        let step = Link::PathStep {
            path,
            component,
            location: None,
            note: text("retries"),
            position: Some(0),
        };
        store.link(&actor, &[step]).unwrap();
        assert_eq!(
            marks(&store, last_event(&store)),
            [
                concerns(component.into()),
                changed(path.into()),
                changed(candidate.into())
            ]
        );

        let NodeView::Path(view) = store.node(path.into()).unwrap() else {
            panic!("a path")
        };
        let input = view.states[0].id;
        let edit = Edit {
            title: None,
            body: None,
            detail: StateEdit {
                location: None,
                trust: Some(Trust::Authenticated),
            },
        };
        store
            .upsert(&actor, &Upsert::State(Change::Edit(input, edit)))
            .unwrap();
        assert_eq!(
            marks(&store, last_event(&store)),
            [
                changed(path.into()),
                changed(input.into()),
                changed(candidate.into())
            ]
        );
        let kept = state(&store, &actor, NewState::Instance);
        let write = access(path, kept, Access::Write, location("decoder/lib.c", 3));
        store.link(&actor, &[write]).unwrap();
        assert_eq!(
            marks(&store, last_event(&store)),
            [
                standing(path.into()),
                standing(candidate.into()),
                standing(kept.into())
            ]
        );

        let (tester, _) = child_agent(&store, skyhook::job::JobState::Running);
        test(&store, &tester, candidate, TestResult::Failed);
        assert_eq!(
            marks(&store, last_event(&store)),
            [standing(candidate.into())]
        );
        assess(&store, other, HIGH);
        assert_eq!(marks(&store, last_event(&store)), [standing(other.into())]);

        let ahead = super::super::tests::flaw(&store, &actor, decoder.component);
        let sibling = super::super::tests::candidate(&store, &actor, path, &[ahead, flaw]);
        assert_eq!(
            marks(&store, last_event(&store)),
            [
                concerns(path.into()),
                concerns(flaw.into()),
                standing(ahead.into()),
                changed(sibling.into())
            ],
            "a root taken moves its flaw alone, a place behind it only concerns its flaw"
        );
        let edit = Edit {
            title: None,
            body: None,
            detail: CandidateEdit {
                actions: None,
                chain: Some(vec![ahead].try_into().unwrap()),
            },
        };
        store
            .upsert(&actor, &Upsert::Candidate(Change::Edit(sibling, edit)))
            .unwrap();
        assert_eq!(
            marks(&store, last_event(&store)),
            [concerns(flaw.into()), changed(sibling.into())]
        );
    }

    /// A path retracted, restored or merged away moves the checks of the states it accesses,
    /// which gain or lose its accesses; marked swept again, it moves none.
    #[test]
    fn a_paths_retraction_restoration_and_merge_move_its_states_checks() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let [kept, other] = ["kept", "other"].map(|name| path(&store, &actor, name, &[decoder]));
        let window = state(&store, &actor, NewState::Instance);
        let at = location("decoder/lib.c", 3);
        let accesses = [
            access(kept, window, Access::Write, at.clone()),
            access(other, window, Access::Read, at),
        ];
        store.link(&actor, &accesses).unwrap();
        for path in [kept, other] {
            sweep(&store, path);
        }
        let owed = || store.run_facts().unwrap().summary.owed(Phase::Check);
        let check = || {
            check(&store, window);
            assert_eq!(owed(), 0);
        };
        let mark = |status| {
            let change = StatusChange::Path(other, status);
            store.set_status(&actor, change, &text("marked")).unwrap();
            owed()
        };
        check();
        assert_eq!(mark(PathStatus::Swept), 0, "a restated sweep");
        assert_eq!(mark(PathStatus::Retracted), 1, "retracted");
        check();
        assert_eq!(mark(PathStatus::Swept), 1, "restored");
        check();
        let merge = Merge {
            node: other.into(),
            into: kept.into(),
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        assert_eq!(owed(), 1, "merged");
    }

    /// A candidate rests on every action it performs, not only the first: a premise of a later
    /// action's path is one its chain rests on, and correcting the input state that path reads
    /// or its steps re-owes the candidate's assessment and verdict and stales its root's
    /// report.
    #[test]
    fn a_later_actions_correction_reowes_its_candidate() {
        let (_session, store, actor) = fixture();
        let first = scenario(&store, &actor, "setter");
        let later = path(&store, &actor, "consumer", &[first.component]);
        let actions = CandidateEdit {
            actions: Some(vec![first.path, later].try_into().unwrap()),
            chain: None,
        };
        let edit = Edit {
            title: None,
            body: None,
            detail: actions,
        };
        let change = Upsert::Candidate(Change::Edit(first.candidate, edit));
        store.upsert(&actor, &change).unwrap();
        let view = || {
            let NodeView::Candidate(view) = store.node(first.candidate.into()).unwrap() else {
                panic!("a candidate")
            };
            view
        };
        let NodeView::Path(consumer) = store.node(later.into()).unwrap() else {
            panic!("a path")
        };
        let held = premise(&store, &actor, PremiseScope::Code);
        let on_later = Link::PathPremise {
            path: later,
            premise: held,
        };
        store.link(&actor, &[on_later]).unwrap();
        assess(&store, first.candidate, HIGH);
        assert_eq!(
            view().confidence,
            Some(ConfidenceTier::Conjectured),
            "a later action's undecided premise is one its chain rests on"
        );
        decide(&store, held, PremiseStatus::Holds);
        let settle = || {
            assess(&store, first.candidate, HIGH);
            verdict(&store, first.candidate, Verdict::Validated);
            store
                .report(&actor, first.flaw, &text("overflow"), &text("report"))
                .unwrap()
        };
        let standing = |finding: FindingId| {
            let view = view();
            let NodeView::Finding(report) = store.node(finding.into()).unwrap() else {
                panic!("a finding")
            };
            (
                view.assessment.unwrap().current,
                view.verdict.unwrap().current,
                report.current,
            )
        };

        let input = Upsert::State(Change::Edit(
            consumer.states[0].id,
            Edit {
                title: None,
                body: None,
                detail: StateEdit {
                    location: None,
                    trust: Some(Trust::Authenticated),
                },
            },
        ));
        let step = Link::PathStep {
            path: later,
            component: first.component,
            location: None,
            note: text("reads it back"),
            position: None,
        };
        let corrections: [Box<dyn Fn()>; 2] = [
            Box::new(|| {
                store.upsert(&actor, &input).unwrap();
            }),
            Box::new(|| store.link(&actor, std::slice::from_ref(&step)).unwrap()),
        ];
        for correct in corrections {
            let finding = settle();
            assert_eq!(standing(finding), (true, true, true));
            correct();
            assert_eq!(standing(finding), (false, false, false));
        }
    }

    /// History pages through the events naming a node, oldest first, each with the nodes it
    /// names, how it bears on them, and the agent that recorded it.
    #[test]
    fn history_pages_a_nodes_events_with_their_marks() {
        let (_session, store, actor) = fixture();
        let decoder = scenario(&store, &actor, "decoder");
        let (candidate, flaw) = (decoder.candidate, decoder.flaw);
        let (child, _, _) = begun(&store, &Begin::Assess(vec![candidate]));
        let vector = HIGH.parse().unwrap();
        store
            .assess(&child, candidate, vector, &text("scored"))
            .unwrap();
        let page = Page::new(0, skyhook::Bounded::new::<1>());
        let first = store.history(Some(candidate.into()), page).unwrap();
        assert_eq!(first.items[0].kind, EventKind::Created);
        assert!(first.items[0].nodes.contains(&Mark {
            node: flaw.into(),
            effect: EventEffect::Standing
        }));
        let after = first.next_after.expect("a full page continues");
        let rest = store
            .history(
                Some(candidate.into()),
                Page::new(after, skyhook::Bounded::new::<2>()),
            )
            .unwrap();
        assert_eq!(rest.items[0].kind, EventKind::Began);
        let assessed = &rest.items[1];
        assert_eq!(
            (assessed.kind, &assessed.agent, assessed.note.as_deref()),
            (EventKind::Assessed, &child, Some("scored"))
        );
        assert_eq!(
            assessed.nodes,
            [Mark {
                node: candidate.into(),
                effect: EventEffect::Standing
            }]
        );
        let all = Listing::drain(|page| store.history(None, page)).unwrap();
        assert_eq!(all.last().unwrap().id, assessed.id);
        assert!(
            all.iter()
                .all(|event| event.agent == actor || event.agent == child)
        );
    }
}
