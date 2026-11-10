//! The run's scope: the components the mapping stage records, outside every scheduled task. A
//! path is in scope while its entry step lies in one of them, by the step's component or by the
//! component owning its file. What only paths entering elsewhere reach owes no work: such a
//! path, a state only they access, a flaw only they step through and no candidate starting in
//! scope holds, a premise only such work relies on, and a candidate starting on one. What
//! later work records outside the scope stays recorded, counted apart. What no path reaches
//! yet is not outside it.

use schemars::JsonSchema;
use serde::Serialize;
use skyhook::db::{Db, enum_column};

use crate::store::*;

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
pub(in crate::store) use mapped;

/// Path `$p` enters outside the scope: its entry step lies in no mapped component, by its own
/// component or by the one owning the file it is located in.
macro_rules! unscoped {
    ($p:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM path_step ue WHERE ue.path=",
            $p,
            " AND ue.position=0 AND NOT ",
            $crate::store::work::scope::mapped!("ue.component"),
            " AND NOT EXISTS (SELECT 1 FROM code_location ul JOIN component_file uf ON \
             uf.file=ul.file WHERE ul.location=ue.location AND ",
            $crate::store::work::scope::mapped!("uf.component"),
            "))"
        )
    };
}
pub(in crate::store) use unscoped;

/// `$reach` selects some path as `path`, and each enters outside the scope.
macro_rules! only_outside {
    ($reach:expr) => {
        concat!(
            "ifnull((SELECT min(",
            $crate::store::work::scope::unscoped!("ro.path"),
            ") FROM (",
            $reach,
            ") ro),0)"
        )
    };
}
pub(in crate::store) use only_outside;

/// State `$s` is reached only from outside the scope: by the unretracted paths accessing it.
macro_rules! state_outside {
    ($s:expr) => {
        $crate::store::work::scope::only_outside!(concat!(
            "SELECT sa.path FROM path_state sa JOIN path sp ON sp.node=sa.path AND \
             sp.status!='retracted' WHERE sa.state=",
            $s
        ))
    };
}
pub(in crate::store) use state_outside;

/// Flaw `$f` is reached only from outside the scope: by the live unretracted paths stepping
/// through its component, and by the entries of the live candidates holding it.
macro_rules! flaw_outside {
    ($f:expr) => {
        $crate::store::work::scope::only_outside!(concat!(
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
pub(in crate::store) use flaw_outside;

/// Candidate `$c` starts outside the scope: its first action, the attacker's entry, does.
macro_rules! candidate_outside {
    ($c:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM candidate_action ce WHERE ce.candidate=",
            $c,
            " AND ce.position=0 AND ",
            $crate::store::work::scope::unscoped!("ce.path"),
            ")"
        )
    };
}
pub(in crate::store) use candidate_outside;

/// What the run holds outside its scope, owing no work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Outside {
    /// Live paths entering outside it.
    pub(crate) paths: u64,
    /// Live candidates starting outside it.
    pub(crate) candidates: u64,
}

/// The run's scope as a task begins on it: the live mapped components and their interfaces,
/// and the deployment premises at their standing, those failing excluding what rests on them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Scope {
    pub(crate) components: Vec<Ref>,
    pub(crate) interfaces: Vec<ScopedInterface>,
    pub(crate) excluded: Vec<Excluded>,
    pub(crate) assumed: Vec<Assumed>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct ScopedInterface {
    pub(crate) interface: InterfaceName,
    pub(crate) component: ComponentId,
}

/// A deployment premise standing failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Excluded {
    pub(crate) premise: PremiseId,
    pub(crate) title: String,
}

/// A deployment premise standing held or undecided.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Assumed {
    pub(crate) premise: PremiseId,
    pub(crate) title: String,
    pub(crate) status: PremiseStatus,
}

/// The live recorded mapped components, as `c` and their node `n`.
macro_rules! mapped_components {
    () => {
        concat!(
            " FROM component c JOIN node n ON n.id=c.node WHERE n.merged_into IS NULL AND \
             c.status='recorded' AND ",
            mapped!("c.node")
        )
    };
}

pub(in crate::store) fn scope(db: &Db) -> Result<Scope> {
    let mut scope = Scope {
        components: rows(
            db,
            concat!(
                "SELECT n.id,n.title",
                mapped_components!(),
                " ORDER BY n.id"
            ),
            Vec::new(),
            |row| {
                Ok(Ref {
                    id: NodeId(row.get(0)?),
                    title: row.get(1)?,
                })
            },
        )?,
        interfaces: rows(
            db,
            concat!(
                "SELECT interface,component FROM component_interface WHERE component IN (SELECT \
                 c.node",
                mapped_components!(),
                ") ORDER BY component,interface"
            ),
            Vec::new(),
            |row| {
                Ok(ScopedInterface {
                    interface: row.get::<String>(0)?.parse()?,
                    component: ComponentId(row.get(1)?),
                })
            },
        )?,
        excluded: Vec::new(),
        assumed: Vec::new(),
    };
    for (premise, title, status) in rows(
        db,
        "SELECT p.node,n.title,p.status FROM premise_standing p JOIN node n ON n.id=p.node WHERE \
         p.scope='deployment' AND n.merged_into IS NULL ORDER BY p.node",
        Vec::new(),
        |row| {
            Ok((
                PremiseId(row.get(0)?),
                row.get::<String>(1)?,
                enum_column::<PremiseStatus>(row, 2)?,
            ))
        },
    )? {
        match status {
            PremiseStatus::Fails => scope.excluded.push(Excluded { premise, title }),
            status => scope.assumed.push(Assumed {
                premise,
                title,
                status,
            }),
        }
    }
    Ok(scope)
}

pub(in crate::store) fn outside(db: &Db) -> Result<Outside> {
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
    use skyhook::job::JobState;

    use super::*;
    use crate::store::queues;
    use crate::store::tests::{
        access, begun, candidate, child_agent, component, end_job, fixture, flaw, location, path,
        premise, scenario, state, text, within,
    };
    use crate::store::work::queues::Spent;

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
            Edge::FlawPremise {
                flaw: decoder.flaw,
                premise: held,
            },
        ];
        store.link(&actor, &links).unwrap();

        let (checker, job, _) = begun(&store, &Begin::Check(Listed::of([shared])));
        let binding = component(&store, &checker, "binding");
        let entry = path(&store, &checker, "binding", &[binding, decoder.component]);
        let inner = path(&store, &checker, "inner", &[decoder.component]);
        let wrapper = flaw(&store, &checker, binding);
        let assumed = premise(&store, &checker, PremiseScope::Code);
        let at = location("binding/lib.c", 5);
        let links = [
            access(entry, shared, Access::Read, at.clone()),
            access(entry, apart, Access::Write, at),
            Edge::FlawPremise {
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
        assert_eq!(owed(Phase::Review), [NodeId::from(decoder.candidate)]);
        let outside = Outside {
            paths: 1,
            candidates: 1,
        };
        assert_eq!(store.synthesis(&[]).unwrap().outside, outside);
        assert_eq!(store.coverage().unwrap().outside, outside);

        let copy = within(&store, &Begin::Sweep(Listed::of([inner])), |agent| {
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

    /// A task begins with the run's scope: the mapped components with their interfaces, never
    /// one a task recorded, and the deployment premises at their standing, a failing one
    /// excluded and the rest assumed.
    #[test]
    fn begin_lists_the_mapped_components_and_the_deployment_premises() {
        let (_session, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let [failing, open] = [(); 2].map(|_| premise(&store, &actor, PremiseScope::Deployment));
        let fails = StatusChange::Premise {
            id: failing,
            status: PremiseStatus::Fails,
        };
        store.set_status(&actor, fails, &text("excluded")).unwrap();
        within(&store, &Begin::Settle(Listed::of([open])), |agent| {
            component(&store, agent, "binding")
        });
        let (settler, _) = child_agent(&store, JobState::Running);
        let begun = store
            .begin(&settler, &Begin::Settle(Listed::of([open])))
            .unwrap();
        let title = "the length is checked".to_owned();
        let scope = Scope {
            components: vec![Ref {
                id: decoder.into(),
                title: "decoder".to_owned(),
            }],
            interfaces: vec![ScopedInterface {
                interface: "decoder-api".parse().unwrap(),
                component: decoder,
            }],
            excluded: vec![Excluded {
                premise: failing,
                title: title.clone(),
            }],
            assumed: vec![Assumed {
                premise: open,
                title,
                status: PremiseStatus::Undecided,
            }],
        };
        assert_eq!(begun.scope, scope);
    }
}
