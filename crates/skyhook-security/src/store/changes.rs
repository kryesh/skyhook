//! What an agent is told of others' changes to the work its job began: for each subject of
//! its job's tasks, as the node it was merged into if it was, the events of its phase's
//! bearing on it since the task began, past the agent's cursor, but no links, which owe the
//! work again after the task instead; and, as a task begins, every one of those since the basis
//! of the subject's previous task in its phase. The bearing leaves out what the
//! agent or one beneath it recorded, as it does for every task of that work. Nothing is stored:
//! the subscription is the task log, and the relation is the one a basis is the latest of.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::Serialize;
use skyhook::db::{Db, enum_column, params};

use super::*;
use crate::store::bearing::bearing;

/// The most recent events one subject's digest names, and nodes it names of each.
const RECENT: usize = 5;
const NODES: usize = 8;

/// Events of `$phase`'s bearing on node `?1` after `?2`, with their kinds. The one bound reaches
/// every arm's index range.
macro_rules! notices {
    ($phase:tt) => {
        concat!(
            "SELECT DISTINCT b.event,e.kind FROM (",
            bearing!($phase, "?1"),
            ") b JOIN event e ON e.id=b.event WHERE b.event>?2 ORDER BY b.event"
        )
    };
}

const fn notices(phase: Phase) -> &'static str {
    match phase {
        Phase::Sweep => notices!("sweep"),
        Phase::Probe => notices!("probe"),
        Phase::Check => notices!("check"),
        Phase::Settle => notices!("settle"),
        Phase::Chain => notices!("chain"),
        Phase::Review => notices!("review"),
        Phase::Test => notices!("test"),
        Phase::Report => notices!("report"),
    }
}

/// An event that moved what a subject's work rests on, with its actor's note and the nodes it
/// moved most.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Notice {
    pub id: EventId,
    pub kind: EventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub nodes: Vec<Touched>,
}

/// A node an event moved, by its title.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Touched {
    pub node: NodeId,
    pub title: String,
    pub effect: EventEffect,
}

/// The changes to one subject's work: how many events, the latest, the kinds seen, the most
/// recent few, oldest first, and every node the older ones moved.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct SubjectChanges {
    pub phase: Phase,
    pub node: NodeId,
    pub title: String,
    pub count: u64,
    pub latest: EventId,
    pub kinds: BTreeSet<EventKind>,
    pub recent: Vec<Notice>,
    /// Each node the events before `recent` moved, once, by the most it moved.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<Touched>,
}

/// Others' changes to what an agent's begun work rests on, one digest per changed subject.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct ResearchChanges {
    pub research_changes: Vec<SubjectChanges>,
}

fn changes(
    db: &Db,
    actor: &AgentId,
    since: Option<EventId>,
) -> Result<Option<(EventId, ResearchChanges)>> {
    let agent = agent_row(db, actor)?;
    // A subject merged away is told of the node it was merged into from its retirement on.
    let subjects = rows(
        db,
        concat!(
            "SELECT tn.phase,coalesce(n.merged_into,n.id),t.began,ifnull(",
            crate::store::bearing::retirement!("n.id"),
            ",0) FROM agent a JOIN task t ON t.job=a.owner_job JOIN task_node tn ON tn.task=t.id \
             JOIN node n ON n.id=tn.node WHERE a.id=?1 ORDER BY t.id,tn.node"
        ),
        params![agent],
        |row| {
            Ok((
                enum_column::<Phase>(row, 0)?,
                NodeId(row.get(1)?),
                EventId(row.get(2)?),
                row.get::<i64>(3)?,
            ))
        },
    )?;
    if subjects.is_empty() {
        return Ok(None);
    }
    let mut found = BTreeMap::<(Phase, NodeId), BTreeMap<EventId, EventKind>>::new();
    for (phase, node, began, merged) in subjects {
        let after = (since.map_or(began, |since| since.max(began)).0).max(merged - 1);
        let mut events = noticed(db, phase, node, EventId(after))?;
        events.retain(|&(_, kind)| !matches!(kind, EventKind::Linked | EventKind::Unlinked));
        if !events.is_empty() {
            found.entry((phase, node)).or_default().extend(events);
        }
    }
    if found.is_empty() {
        return Ok(None);
    }
    let through = stored(db, "SELECT max(id) FROM event", Vec::new(), |row| {
        Ok(EventId(row.get(0)?))
    })?;
    let mut research_changes = Vec::with_capacity(found.len());
    for ((phase, node), events) in found {
        research_changes.extend(digest(db, phase, node, &events)?);
    }
    Ok(Some((through, ResearchChanges { research_changes })))
}

/// The events of `phase`'s bearing on `node` after `after`, oldest first.
pub(super) fn noticed(
    db: &Db,
    phase: Phase,
    node: NodeId,
    after: EventId,
) -> Result<Vec<(EventId, EventKind)>> {
    rows(db, notices(phase), params![node, after], |row| {
        Ok((EventId(row.get(0)?), enum_column::<EventKind>(row, 1)?))
    })
}

/// The digest of `events` on `phase`'s work on `node`; none when there are none.
pub(super) fn digest(
    db: &Db,
    phase: Phase,
    node: NodeId,
    events: &BTreeMap<EventId, EventKind>,
) -> Result<Option<SubjectChanges>> {
    let Some((&latest, _)) = events.last_key_value() else {
        return Ok(None);
    };
    let older: Vec<EventId> = events.keys().rev().skip(RECENT).copied().collect();
    let earlier = rows(
        db,
        "SELECT en.node,n.title,en.effect,max(k.rank) FROM json_each(?1) j JOIN event_node en ON \
         en.event=j.value JOIN event_effect k ON k.name=en.effect JOIN node n ON n.id=en.node \
         WHERE en.effect!='concerns' GROUP BY en.node ORDER BY max(k.rank) DESC,en.node",
        params![json_list(&older)?],
        touched,
    )?;
    let mut recent = Vec::with_capacity(RECENT);
    for (&id, &kind) in events.iter().rev().take(RECENT).rev() {
        let nodes = first_rows(
            db,
            "SELECT en.node,n.title,en.effect FROM event_node en JOIN event_effect k ON \
             k.name=en.effect JOIN node n ON n.id=en.node WHERE en.event=?1 AND \
             en.effect!='concerns' ORDER BY k.rank DESC,en.node",
            params![id],
            NODES,
            touched,
        )?;
        let note = stored(
            db,
            "SELECT note FROM event WHERE id=?1",
            params![id],
            |row| Ok(row.get::<Option<String>>(0)?),
        )?;
        recent.push(Notice {
            id,
            kind,
            note,
            nodes,
        });
    }
    let title = stored(
        db,
        "SELECT title FROM node WHERE id=?1",
        params![node],
        |row| Ok(row.get::<String>(0)?),
    )?;
    Ok(Some(SubjectChanges {
        phase,
        node,
        title,
        count: events.len() as u64,
        latest,
        kinds: events.values().copied().collect(),
        recent,
        earlier,
    }))
}

/// A node an event moved, as a row of its id, title and effect.
fn touched(row: &libsql::Row) -> Result<Touched> {
    Ok(Touched {
        node: NodeId(row.get(0)?),
        title: row.get(1)?,
        effect: enum_column(row, 2)?,
    })
}

impl Store {
    /// What others changed of what `actor`'s begun work rests on after `since`, or since each
    /// of its tasks began, with the latest event the read saw; none when nothing did.
    pub fn changes(
        &self,
        actor: &AgentId,
        since: Option<EventId>,
    ) -> Result<Option<(EventId, ResearchChanges)>> {
        self.read(|db| changes(db, actor, since))
    }
}

#[cfg(test)]
mod tests {
    use skyhook::job::JobState;

    use super::{
        super::tests::{
            HIGH, assess, begun, child_of, decide, fixture, last_event, premise, scenario, text,
            validated,
        },
        *,
    };

    fn evidence(store: &Store, actor: &AgentId, node: impl Into<NodeId>, stance: Stance) {
        let item = NewEvidence {
            location: None,
            environment: None,
            stance,
            note: text("seen"),
        };
        store.evidence(actor, node.into(), &[item]).unwrap();
    }

    /// A reviewer is told, once, of another agent's change to what its review rests on, with the
    /// nodes it moved most first; a resumed agent with no cursor gets everything since it began. It
    /// is not told of its own writes, its child's, or an event that only concerns its subject.
    #[test]
    fn an_agent_is_told_of_others_changes_to_its_work_once() {
        let (_session, store, root) = fixture();
        let decoder = scenario(&store, &root, "decoder");
        let (candidate, flaw) = (decoder.candidate, decoder.flaw);
        let code = premise(&store, &root, PremiseScope::Code);
        let link = Link::FlawPremise {
            flaw,
            premise: code,
        };
        store.link(&root, &[link]).unwrap();
        assess(&store, candidate, HIGH);
        let (reviewer, _, _) = begun(&store, &Begin::Review(vec![candidate]));
        assert_eq!(store.changes(&reviewer, None).unwrap(), None);

        decide(&store, code, PremiseStatus::Holds);
        let decision = last_event(&store);
        let (through, told) = store.changes(&reviewer, None).unwrap().unwrap();
        let mark = |node: NodeId, title: &str, effect| Touched {
            node,
            title: title.to_owned(),
            effect,
        };
        assert_eq!(
            (through, told),
            (
                decision,
                ResearchChanges {
                    research_changes: vec![SubjectChanges {
                        phase: Phase::Review,
                        node: candidate.into(),
                        title: "overflow".to_owned(),
                        count: 1,
                        latest: decision,
                        kinds: BTreeSet::from([EventKind::Status]),
                        recent: vec![Notice {
                            id: decision,
                            kind: EventKind::Status,
                            note: Some("settled".to_owned()),
                            nodes: vec![
                                mark(code.into(), "the length is checked", EventEffect::Changed),
                                mark(flaw.into(), "unbounded copy", EventEffect::Standing),
                                mark(candidate.into(), "overflow", EventEffect::Standing),
                            ],
                        }],
                        earlier: Vec::new(),
                    }],
                }
            )
        );
        assert_eq!(store.changes(&reviewer, Some(decision)).unwrap(), None);

        let (helper, _) = child_of(&store, &reviewer, JobState::Running);
        evidence(&store, &reviewer, candidate, Stance::Contradicts);
        evidence(&store, &helper, flaw, Stance::Contradicts);
        evidence(&store, &root, candidate, Stance::Supports);
        begun(&store, &Begin::Test(vec![candidate]));
        assert_eq!(store.changes(&reviewer, Some(decision)).unwrap(), None);
        assert_eq!(
            store.changes(&root, None).unwrap(),
            None,
            "the root has no task"
        );
    }

    /// Running work is not told of a link on what it rests on, which owes it again after the
    /// task, but is told of an edit.
    #[test]
    fn running_work_is_told_of_decisions_not_links() {
        let (_session, store, root) = fixture();
        let decoder = scenario(&store, &root, "decoder");
        let (sweeper, _, _) = begun(&store, &Begin::Sweep(vec![decoder.path]));
        let step = Link::PathStep {
            path: decoder.path,
            component: decoder.component,
            location: None,
            note: text("returns"),
            position: None,
        };
        store.link(&root, &[step]).unwrap();
        assert_eq!(store.changes(&sweeper, None).unwrap(), None);
        let edit = Upsert::Path(Change::Edit(
            decoder.path,
            Edit {
                title: None,
                body: Some(text("socket to inflate")),
                detail: (),
            },
        ));
        store.upsert(&root, &edit).unwrap();
        let told = store.changes(&sweeper, None).unwrap().unwrap().1;
        let kinds: Vec<_> = (told.research_changes.iter())
            .map(|s| s.kinds.clone())
            .collect();
        assert_eq!(kinds, [BTreeSet::from([EventKind::Edited])]);
    }

    /// A subject merged away after its task began is told of changes to the node it was
    /// merged into from the merge on, however its chain's history is marked after.
    #[test]
    fn a_merged_subject_is_told_of_its_survivors_changes() {
        let (_session, store, root) = fixture();
        let (merged, kept) = (
            scenario(&store, &root, "a"),
            scenario(&store, &root, "b").candidate,
        );
        let (merged, merged_flaw) = (merged.candidate, merged.flaw);
        let (reviewer, _, _) = begun(&store, &Begin::Review(vec![merged]));
        evidence(&store, &root, kept, Stance::Contradicts);
        let merge = Merge {
            node: merged.into(),
            into: kept.into(),
            note: text("same"),
        };
        store.merge(&root, &merge).unwrap();
        let merged_at = last_event(&store);
        evidence(&store, &root, kept, Stance::Contradicts);
        let disputed = last_event(&store);
        let rechained = Merge {
            node: merged_flaw.into(),
            into: scenario(&store, &root, "c").flaw.into(),
            note: text("same"),
        };
        store.merge(&root, &rechained).unwrap();
        let told = store.changes(&reviewer, None).unwrap().unwrap().1;
        assert_eq!(
            (told.research_changes.iter())
                .map(|subject| (subject.node, subject.recent.iter().map(|n| n.id).collect()))
                .collect::<Vec<(NodeId, Vec<EventId>)>>(),
            [(kept.into(), vec![merged_at, disputed])]
        );
    }

    /// Each subject is told what its own phase rests on: a settling look of a dispute, a test
    /// of a new environment. Every changed subject has a digest: its count, latest event and
    /// kinds, with its newest few events and every node the older ones moved.
    #[test]
    fn notices_follow_each_phases_bearing_as_a_digest_per_subject() {
        let (_session, store, root) = fixture();
        let code = premise(&store, &root, PremiseScope::Code);
        let (settler, _, _) = begun(&store, &Begin::Settle(vec![code]));
        evidence(&store, &root, code, Stance::Contradicts);
        let disputed = store.changes(&settler, None).unwrap().unwrap().1;
        assert_eq!(disputed.research_changes[0].phase, Phase::Settle);

        let candidate = validated(&store, &root, "decoder").candidate;
        let (tester, _, _) = begun(&store, &Begin::Test(vec![candidate]));
        let environment = NewEnvironment {
            instructions: text("start the container"),
            candidate: None,
            serves: Serves::default(),
        };
        store.environment(&root, &environment).unwrap();
        let told = store.changes(&tester, None).unwrap().unwrap().1;
        let kinds: Vec<EventKind> = (told.research_changes.iter())
            .flat_map(|subject| subject.recent.iter().map(|event| event.kind))
            .collect();
        assert_eq!(kinds, [EventKind::Environment]);

        let quiet = premise(&store, &root, PremiseScope::Code);
        let (bundled, _, _) = begun(&store, &Begin::Settle(vec![code, quiet]));
        for status in [PremiseStatus::Holds, PremiseStatus::Fails].repeat(12) {
            let change = StatusChange::Premise(code, status);
            store.set_status(&root, change, &text("flip")).unwrap();
        }
        evidence(&store, &root, quiet, Stance::Contradicts);
        let (through, digest) = store.changes(&bundled, None).unwrap().unwrap();
        assert_eq!(through, last_event(&store));
        let [busy, other] = &digest.research_changes[..] else {
            panic!("every changed subject: {digest:?}")
        };
        assert_eq!(
            (busy.node, busy.count, busy.latest, &busy.kinds),
            (
                NodeId::from(code),
                24,
                EventId(through.get() - 1),
                &BTreeSet::from([EventKind::Status])
            )
        );
        assert_eq!(busy.recent.len(), RECENT);
        assert_eq!(busy.recent.last().unwrap().id, busy.latest);
        let earlier: Vec<NodeId> = busy.earlier.iter().map(|touched| touched.node).collect();
        assert_eq!(
            earlier,
            [NodeId::from(code)],
            "the older events' nodes, once"
        );
        assert_eq!(
            (other.node, other.count, other.earlier.len()),
            (NodeId::from(quiet), 1, 0)
        );
    }
}
