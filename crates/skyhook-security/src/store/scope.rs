//! The run's scope: the components the mapping stage records, outside every scheduled task. A
//! path is in scope while its entry step lies in one of them, by the step's component or by the
//! component owning its file. What only paths entering elsewhere reach owes no work: such a
//! path, a state only they access, a flaw only they step through and no candidate starting in
//! scope holds, a premise only such work relies on, and a candidate starting on one. Nothing
//! is refused: what later work records outside the scope stays recorded, counted apart. What
//! no path reaches yet is not outside it.

use schemars::JsonSchema;
use serde::Serialize;
use skyhook::db::Db;

use super::*;

/// Component `$c` was recorded outside every scheduled task, by the mapping stage, or absorbs
/// one that was: the event creating it, the first naming it, is within no task. A merge leaves
/// the survivor holding the duplicate's files and steps, so it holds its place in the scope too.
macro_rules! mapped {
    ($c:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM node ma WHERE (ma.id=",
            $c,
            " OR ma.merged_into=",
            $c,
            ") AND NOT EXISTS (SELECT 1 FROM event_task mt WHERE mt.event=(SELECT min(mn.event) \
             FROM event_node mn WHERE mn.node=ma.id)))"
        )
    };
}
pub(super) use mapped;

/// Path `$p` enters outside the scope: its entry step lies in no mapped component, by its own
/// component or by the one owning the file it is located in.
macro_rules! unscoped {
    ($p:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM path_step ue WHERE ue.path=",
            $p,
            " AND ue.position=0 AND NOT ",
            $crate::store::scope::mapped!("ue.component"),
            " AND NOT EXISTS (SELECT 1 FROM code_location ul JOIN component_file uf ON \
             uf.file=ul.file WHERE ul.location=ue.location AND ",
            $crate::store::scope::mapped!("uf.component"),
            "))"
        )
    };
}
pub(super) use unscoped;

/// `$reach` selects some path as `path`, and each enters outside the scope.
macro_rules! only_outside {
    ($reach:expr) => {
        concat!(
            "ifnull((SELECT min(",
            $crate::store::scope::unscoped!("ro.path"),
            ") FROM (",
            $reach,
            ") ro),0)"
        )
    };
}
pub(super) use only_outside;

/// State `$s` is reached only from outside the scope: by the unretracted paths accessing it.
macro_rules! state_outside {
    ($s:expr) => {
        $crate::store::scope::only_outside!(concat!(
            "SELECT sa.path FROM path_state sa JOIN path sp ON sp.node=sa.path AND \
             sp.status!='retracted' WHERE sa.state=",
            $s
        ))
    };
}
pub(super) use state_outside;

/// Flaw `$f` is reached only from outside the scope: by the live unretracted paths stepping
/// through its component, and by the entries of the live candidates holding it.
macro_rules! flaw_outside {
    ($f:expr) => {
        $crate::store::scope::only_outside!(concat!(
            "SELECT fs.path FROM flaw ff JOIN path_step fs ON fs.component=ff.component JOIN path \
             fp ON fp.node=fs.path AND fp.status!='retracted' JOIN node fo ON fo.id=fs.path AND \
             fo.merged_into IS NULL WHERE ff.node=",
            $f,
            " UNION ALL SELECT fa.path FROM candidate_flaw fc JOIN candidate_action fa ON \
             fa.candidate=fc.candidate AND fa.position=0 JOIN node fm ON fm.id=fc.candidate AND \
             fm.merged_into IS NULL WHERE fc.flaw=",
            $f
        ))
    };
}
pub(super) use flaw_outside;

/// Candidate `$c` starts outside the scope: its first action, the attacker's entry, does.
macro_rules! candidate_outside {
    ($c:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM candidate_action ce WHERE ce.candidate=",
            $c,
            " AND ce.position=0 AND ",
            $crate::store::scope::unscoped!("ce.path"),
            ")"
        )
    };
}
pub(super) use candidate_outside;

/// What the run holds outside its scope, owing no work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Outside {
    /// Live paths entering outside it.
    pub paths: u64,
    /// Live candidates starting outside it.
    pub candidates: u64,
}

pub(super) fn outside(db: &Db) -> Result<Outside> {
    stored(
        db,
        concat!(
            "SELECT (SELECT count(*) FROM path p JOIN node n ON n.id=p.node WHERE n.merged_into IS \
             NULL AND p.status!='retracted' AND ",
            unscoped!("p.node"),
            "),(SELECT count(*) FROM candidate c JOIN node n ON n.id=c.node WHERE n.merged_into IS \
             NULL AND ",
            candidate_outside!("c.node"),
            ")"
        ),
        Vec::new(),
        |row| {
            Ok(Outside {
                paths: row.get(0)?,
                candidates: row.get(1)?,
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            queues::{self, Spent},
            tests::{
                access, begun, candidate, component, end_job, fixture, flaw, location, path,
                premise, scenario, state, text, within,
            },
        },
        *,
    };

    /// The mapping stage's components are the run's scope. A check may still record a
    /// component, a path entering through it, a flaw there resting on a premise of its own and
    /// a candidate starting on that path: they stay recorded, counted outside the scope, and owe
    /// nothing, nor does a state only that path accesses. A path the check records entering a
    /// mapped component is in scope, and a state an in-scope path shares with the outside one
    /// is checked as before. A mapped component merged into one a later task recorded keeps its
    /// paths in scope.
    #[test]
    fn work_reached_only_from_outside_the_mapped_components_owes_nothing() {
        let (_session, store, actor) = fixture();
        let decoder = scenario(&store, &actor, "decoder");
        let held = premise(&store, &actor, PremiseScope::Code);
        let [shared, apart] = [(); 2].map(|_| state(&store, &actor, NewState::Instance));
        let links = [
            access(
                decoder.path,
                shared,
                Access::Write,
                location("decoder/lib.c", 3),
            ),
            Link::FlawPremise {
                flaw: decoder.flaw,
                premise: held,
            },
        ];
        store.link(&actor, &links).unwrap();

        let (checker, job, _) = begun(&store, &Begin::Check(vec![shared]));
        let binding = component(&store, &checker, "binding");
        let entry = path(&store, &checker, "binding", &[binding, decoder.component]);
        let inner = path(&store, &checker, "inner", &[decoder.component]);
        let wrapper = flaw(&store, &checker, binding);
        let assumed = premise(&store, &checker, PremiseScope::Code);
        let at = location("binding/lib.c", 5);
        let links = [
            access(entry, shared, Access::Read, at.clone()),
            access(entry, apart, Access::Write, at),
            Link::FlawPremise {
                flaw: wrapper,
                premise: assumed,
            },
        ];
        store.link(&checker, &links).unwrap();
        candidate(&store, &checker, entry, &[decoder.flaw]);
        end_job(&store, job);

        let owed = |phase| {
            (store.read(|db| queues::owed(db, phase, &Spent::default())))
                .unwrap()
                .into_iter()
                .map(|owed| owed.node)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            owed(Phase::Sweep),
            [NodeId::from(decoder.path), inner.into()]
        );
        assert_eq!(owed(Phase::Check), [NodeId::from(shared)]);
        assert_eq!(owed(Phase::Settle), [NodeId::from(held)]);
        assert!(owed(Phase::Chain).is_empty());
        assert_eq!(owed(Phase::Assess), [NodeId::from(decoder.candidate)]);
        let outside = Outside {
            paths: 1,
            candidates: 1,
        };
        assert_eq!(store.synthesis(&[]).unwrap().outside, outside);
        assert_eq!(store.coverage().unwrap().outside, outside);

        let copy = within(&store, &Begin::Sweep(vec![inner]), |agent| {
            component(&store, agent, "copy")
        });
        let merge = Merge {
            node: decoder.component.into(),
            into: copy.into(),
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        assert_eq!(store.coverage().unwrap().outside, outside);
    }
}
