//! What an agent is told of others' changes to the work its job began: for each subject of
//! its job's tasks, as the node it was merged into if it was, the events of its phase's
//! bearing on it since the task began, past the agent's cursor. The bearing leaves out what the
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

/// Events of `$phase`'s bearing on node `?1` after `?2` and from `?3` on, with their kinds.
macro_rules! notices {
    ($phase:tt) => {
        concat!(
            "SELECT DISTINCT b.event,e.kind FROM (",
            bearing!($phase, "?1"),
            ") b JOIN event e ON e.id=b.event WHERE b.event>?2 AND b.event>=?3 ORDER BY b.event"
        )
    };
}

const fn notices(phase: Phase) -> &'static str {
    match phase {
        Phase::Sweep => notices!("sweep"),
        Phase::Settle => notices!("settle"),
        Phase::Chain => notices!("chain"),
        Phase::Assess => notices!("assess"),
        Phase::Review => notices!("review"),
        Phase::Test => notices!("test"),
        Phase::Report => notices!("report"),
    }
}

/// An event that moved what a subject's work rests on, with the nodes it moved most.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Notice {
    pub id: EventId,
    pub kind: EventKind,
    pub nodes: Vec<Mark>,
}

/// The changes to one subject's work: how many events, the latest, the kinds seen, and the
/// most recent few, oldest first.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct SubjectChanges {
    pub phase: Phase,
    pub node: NodeId,
    pub count: u64,
    pub latest: EventId,
    pub kinds: BTreeSet<EventKind>,
    pub recent: Vec<Notice>,
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
    // A subject merged away is told of the node it was merged into from the merge retiring it
    // on: the latest merge changing it, as it changed none after.
    let subjects = rows(
        db,
        "SELECT tn.phase,coalesce(n.merged_into,n.id),t.began,CASE WHEN n.merged_into IS NOT NULL \
         THEN (SELECT max(en.event) FROM event_node en JOIN event e ON e.id=en.event WHERE \
         en.node=n.id AND en.effect='changed' AND e.kind='merged') ELSE 0 END FROM agent a JOIN \
         task t ON t.job=a.owner_job JOIN task_node tn ON tn.task=t.id JOIN node n ON \
         n.id=tn.node WHERE a.id=?1 ORDER BY t.id,tn.node",
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
        let after = since.map_or(began, |since| since.max(began));
        let events = rows(db, notices(phase), params![node, after, merged], |row| {
            Ok((EventId(row.get(0)?), enum_column::<EventKind>(row, 1)?))
        })?;
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
        let Some((&latest, _)) = events.last_key_value() else {
            continue;
        };
        let mut recent = Vec::with_capacity(RECENT);
        for (&id, &kind) in events.iter().rev().take(RECENT).rev() {
            let nodes = rows(
                db,
                "SELECT en.node,en.effect FROM event_node en JOIN event_effect k ON \
                 k.name=en.effect WHERE en.event=?1 AND en.effect IN ('standing','changed') \
                 ORDER BY k.rank DESC,en.node LIMIT ?2",
                params![id, NODES],
                |row| {
                    Ok(Mark {
                        node: NodeId(row.get(0)?),
                        effect: enum_column(row, 1)?,
                    })
                },
            )?;
            recent.push(Notice { id, kind, nodes });
        }
        research_changes.push(SubjectChanges {
            phase,
            node,
            count: events.len() as u64,
            latest,
            kinds: events.values().copied().collect(),
            recent,
        });
    }
    Ok(Some((through, ResearchChanges { research_changes })))
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
            HIGH, assess, begun, child_of, fixture, last_event, premise, scenario, text, validated,
        },
        *,
    };

    fn evidence(store: &Store, actor: &AgentId, node: impl Into<NodeId>, stance: Stance) {
        let item = NewEvidence {
            location: None,
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
        assess(&store, &root, candidate, HIGH);
        let (reviewer, _, _) = begun(&store, &Begin::Review(vec![candidate]));
        assert_eq!(store.changes(&reviewer, None).unwrap(), None);

        let decided = StatusChange::Premise(code, PremiseStatus::Holds);
        store.set_status(&root, decided, &text("checked")).unwrap();
        let decision = last_event(&store);
        let (through, told) = store.changes(&reviewer, None).unwrap().unwrap();
        let mark = |node: NodeId, effect| Mark { node, effect };
        assert_eq!(
            (through, told),
            (
                decision,
                ResearchChanges {
                    research_changes: vec![SubjectChanges {
                        phase: Phase::Review,
                        node: candidate.into(),
                        count: 1,
                        latest: decision,
                        kinds: BTreeSet::from([EventKind::Status]),
                        recent: vec![Notice {
                            id: decision,
                            kind: EventKind::Status,
                            nodes: vec![
                                mark(code.into(), EventEffect::Changed),
                                mark(flaw.into(), EventEffect::Standing),
                                mark(candidate.into(), EventEffect::Standing),
                            ],
                        }],
                    }],
                }
            )
        );
        assert_eq!(store.changes(&reviewer, Some(decision)).unwrap(), None);

        let (helper, _) = child_of(&store, &reviewer, JobState::Running);
        evidence(&store, &reviewer, candidate, Stance::Contradicts);
        evidence(&store, &helper, flaw, Stance::Contradicts);
        evidence(&store, &root, candidate, Stance::Supports);
        begun(&store, &Begin::Assess(vec![candidate]));
        assert_eq!(store.changes(&reviewer, Some(decision)).unwrap(), None);
        assert_eq!(
            store.changes(&root, None).unwrap(),
            None,
            "the root has no task"
        );
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
        let merge = Merge::Candidate {
            candidate: merged,
            into: kept,
            note: text("same"),
        };
        store.merge(&root, &merge).unwrap();
        let merged_at = last_event(&store);
        evidence(&store, &root, kept, Stance::Contradicts);
        let disputed = last_event(&store);
        let rechained = Merge::Flaw {
            flaw: merged_flaw,
            into: scenario(&store, &root, "c").flaw,
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
    /// kinds, with its newest few events.
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
        assert_eq!((other.node, other.count), (NodeId::from(quiet), 1));
    }
}
