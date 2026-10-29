//! Following a flaw's consequences: a flaw that makes its code report a status, error or
//! state for the wrong reason owes a trace of every consumer of that value, and a flaw that
//! breaks a guarantee or premise undermines it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, params};

use super::*;

/// A guarantee or premise: what a flaw may break, and what a question or follow-up asks about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Invariant {
    Guarantee(GuaranteeId),
    Premise(PremiseId),
}

impl Invariant {
    pub(super) fn of_columns(guarantee: Option<i64>, premise: Option<i64>) -> Option<Self> {
        guarantee
            .map(|id| Self::Guarantee(GuaranteeId(id)))
            .or(premise.map(|id| Self::Premise(PremiseId(id))))
    }

    pub(super) const fn guarantee(self) -> Option<GuaranteeId> {
        match self {
            Self::Guarantee(id) => Some(id),
            Self::Premise(_) => None,
        }
    }

    pub(super) const fn premise(self) -> Option<PremiseId> {
        match self {
            Self::Premise(id) => Some(id),
            Self::Guarantee(_) => None,
        }
    }

    pub(super) fn known(self, db: &Db) -> Result<()> {
        match self {
            Self::Guarantee(id) => scope::known(db, id),
            Self::Premise(id) => premises::subject(db, id).map(drop),
        }
    }
}

/// Plan the trace a misreporting flaw owes: every consumer of the value it gets wrong.
pub(super) fn trace(db: &Db, actor: &AgentId, flaw: FlawId, misreport: Misreport) -> Result<()> {
    plan::owed(
        db,
        actor,
        &UnitWork::FollowUp {
            question: format!(
                "Flaw #{flaw} makes its code report a {misreport} for the wrong reason: trace that {misreport} to every immediate caller, retry loop and stateful consumer, and decide what each does with it."
            )
            .parse()?,
            stopping: "Every consumer is recorded as checked: a new flaw, a candidate, contradicting evidence, an undermines link, or evidence that a guarantee holds despite the flaw."
                .parse()?,
            candidate: None,
            flaw: Some(flaw),
            invariants: Vec::new(),
        },
    )
    .map(drop)
}

impl Store {
    /// Records that `flaw` breaks `invariant`, a guarantee or premise. It counts as
    /// contradicting it: a premise moves to a new revision, so a condition's resolution and the
    /// verdicts resting on it are owed again, and a condition gating a scenario contradicts that
    /// scenario, so its tests no longer stand; a check naming a guarantee is broken. Restating
    /// a link records nothing.
    pub fn undermine(
        &self,
        actor: &AgentId,
        flaw: FlawId,
        invariant: Invariant,
        note: &Text,
    ) -> Result<UnderminingId> {
        self.write(|db| {
            flaws::live(db, flaw)?;
            invariant.known(db)?;
            let (guarantee, premise) = (invariant.guarantee(), invariant.premise());
            let columns = params![flaw.0, guarantee.map(|id| id.0), premise.map(|id| id.0)];
            if let Some(id) = optional(
                db,
                "SELECT id FROM undermining WHERE flaw=?1 AND guarantee IS ?2 AND premise IS ?3",
                columns.clone(),
                |row| Ok(UnderminingId(row.get(0)?)),
            )? {
                return Ok(id);
            }
            let actor_id = actor_row(db, actor)?;
            let id = UnderminingId(db.insert(
                "INSERT INTO undermining(flaw,guarantee,premise,note,actor,at) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    flaw.0,
                    guarantee.map(|id| id.0),
                    premise.map(|id| id.0),
                    note,
                    actor_id,
                    now()
                ],
            )?);
            if let Some(premise) = premise {
                premises::touch(db, actor, premise)?;
                premises::contradict(db, premise)?;
            }
            audit(
                db,
                actor,
                EventKind::Recorded,
                Detail::Record(EventRecord::Undermining(id)),
                &[Subject::Flaw(flaw)],
            )?;
            Ok(id)
        })
    }
}

pub(super) fn undermined_by_flaw(db: &Db, flaw: FlawId) -> Result<Vec<Invariant>> {
    rows(
        db,
        "SELECT guarantee,premise FROM undermining WHERE flaw=?1 ORDER BY id",
        params![flaw.0],
        |row| {
            Ok(Invariant::of_columns(row.get(0)?, row.get(1)?)
                .expect("one invariant column is set"))
        },
    )
}

/// The live flaws undermining a guarantee or premise.
pub(super) fn underminers(db: &Db, invariant: Invariant) -> Result<Vec<FlawId>> {
    rows(
        db,
        "SELECT u.flaw FROM undermining u JOIN flaw f ON f.id=u.flaw WHERE f.merged_into IS NULL AND u.guarantee IS ?1 AND u.premise IS ?2 ORDER BY u.id",
        params![
            invariant.guarantee().map(|id| id.0),
            invariant.premise().map(|id| id.0)
        ],
        |row| Ok(FlawId(row.get(0)?)),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{component, evidence_at, fixture, new_flaw, new_guarantee, text, units},
        *,
    };

    /// A status returned for the wrong reason owes a trace of its consumers, and a link
    /// records once that the flaw breaks a guarantee relying on its usual meaning.
    #[test]
    fn a_misreported_status_is_traced_and_undermines_what_it_breaks() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let follow_ups = || {
            (units(&store, UnitKind::FollowUp).into_iter())
                .map(|unit| match unit.work {
                    UnitWork::FollowUp {
                        flaw, invariants, ..
                    } => (flaw, invariants),
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>()
        };
        let mut misreport = new_flaw(decoder, "an impossible switch reports that input ran out");
        misreport.misreports = Some(Misreport::Status);
        misreport.evidence = vec![evidence_at("decode.c", 1100)];
        let flaw = store.add_flaw(&actor, &misreport).unwrap();
        store
            .add_flaw(&actor, &new_flaw(decoder, "an unrelated copy"))
            .unwrap();
        assert_eq!(follow_ups(), [(Some(flaw), Vec::new())]);

        let evidence = vec![evidence_at("decode.c", 2463)];
        let tail = store
            .add_guarantee(
                &actor,
                decoder,
                &new_guarantee(InvariantKind::State, evidence),
            )
            .unwrap();
        let note = text("the retry copies a tail the switch never consumed");
        let link = store
            .undermine(&actor, flaw, Invariant::Guarantee(tail), &note)
            .unwrap();
        assert_eq!(
            store
                .undermine(&actor, flaw, Invariant::Guarantee(tail), &note)
                .unwrap(),
            link
        );
        let dossier = store.dossier(decoder).unwrap();
        assert_eq!(dossier.guarantees[0].undermined_by, [flaw]);
        assert_eq!(
            dossier.guarantees[0].evidence[0].location.function,
            Some(text("DecodeStream"))
        );
    }
}
