//! Implementation components and their dossiers: what each enforces, where its contracts
//! stand, what was left unexamined, and which paths reach it.

use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, params};

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewComponent {
    pub name: Text,
    pub description: Text,
    /// Files this component owns. A file belongs to one component; naming another's file
    /// moves it here until a completed sweep covers it.
    pub files: Vec<Text>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Component {
    pub id: ComponentId,
    pub name: String,
    pub description: String,
    /// The latest sweep's outcome; none until one is recorded.
    pub swept: Option<Outcome>,
    pub coverage: ComponentCoverage,
}

/// An argued invariant of a component: its kind, its statement, and the preconditions
/// under which the argument holds.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewGuarantee {
    pub kind: InvariantKind,
    pub statement: Text,
    pub preconditions: Text,
    #[serde(default)]
    pub function: Option<Text>,
    #[serde(default)]
    pub evidence: Vec<NewEvidence>,
    /// For a bound or progress argued across the attempts a retry status asks for: that
    /// status. The evidence then covers every site of it the component records.
    #[serde(default)]
    pub retry_status: Option<Text>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Guarantee {
    pub id: GuaranteeId,
    pub component: ComponentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    pub kind: InvariantKind,
    pub statement: String,
    pub preconditions: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_status: Option<String>,
    pub evidence: Vec<Evidence>,
    /// The live flaws recorded as breaking it.
    pub undermined_by: Vec<FlawId>,
}

/// What a deferred region belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DeferredSubject {
    Component(ComponentId),
    Path(PathId),
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Deferred {
    pub id: DeferredId,
    pub subject: DeferredSubject,
    pub location: Option<Location>,
    pub reason: String,
    /// The examination that later covered it.
    pub settled: Option<Settlement>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Settlement {
    pub note: String,
    /// The guarantees it rests on, each with evidence inside the region.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guarantees: Vec<GuaranteeId>,
}

/// Everything recorded about a component, for reviewers to reuse instead of
/// rediscovering it.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Dossier {
    pub component: Component,
    pub files: Vec<String>,
    pub reached_by: Vec<PathId>,
    /// Its sweep units with their outcomes.
    pub sweeps: Vec<PlanUnit>,
    pub guarantees: Vec<Guarantee>,
    /// Its contracts by their latest checks.
    pub checks: Vec<Contract>,
    /// The retry-status sites in its files.
    pub retry_sites: Vec<RetrySite>,
    pub deferred: Vec<Deferred>,
    pub flaws: Vec<FlawSummary>,
}

/// The latest outcome of component `c`'s sweeps.
macro_rules! latest_sweep {
    () => {
        "(SELECT o.kind FROM outcome o JOIN plan_unit u ON u.id=o.unit WHERE u.component=c.id \
         ORDER BY o.id DESC LIMIT 1)"
    };
}
pub(super) use latest_sweep;

/// A component `c`'s coverage, decoded as [`ComponentCoverage`]: inspected while its latest
/// sweep completed and sweeps covered every file it holds now.
macro_rules! component_coverage {
    () => {
        concat!(
            "CASE WHEN ",
            components::latest_sweep!(),
            "='completed' AND NOT EXISTS (SELECT 1 FROM component_file cf WHERE \
             cf.component=c.id AND NOT EXISTS (SELECT 1 FROM swept_file WHERE file=cf.file)) \
             THEN 'inspected' WHEN EXISTS (SELECT 1 FROM path_reach WHERE component=c.id) \
             THEN 'reached' ELSE 'mapped' END"
        )
    };
}
pub(super) use component_coverage;

/// Selects components `c`, decoded by [`component_row`], then any further columns.
macro_rules! component_select {
    ($($more:literal)?) => {
        concat!(
            "SELECT c.id,c.name,c.description,",
            components::latest_sweep!(),
            " AS swept,",
            components::component_coverage!(),
            $(",", $more,)?
            " FROM component c WHERE "
        )
    };
}
pub(super) use component_select;

pub(super) fn component_row(row: &Row) -> Result<Component> {
    Ok(Component {
        id: ComponentId(row.get(0)?),
        name: row.get(1)?,
        description: row.get(2)?,
        swept: skyhook::db::optional_enum_column(row, 3)?,
        coverage: enum_column(row, 4)?,
    })
}

pub(super) fn component(db: &Db, id: ComponentId) -> Result<Component> {
    one(
        db,
        concat!(component_select!(), "c.id=?1"),
        id,
        component_row,
    )
}

macro_rules! deferred_select {
    () => {
        concat!(
            "SELECT d.id,d.component,d.path,d.reason,s.note,",
            scope::location_columns!("l"),
            " FROM deferred d LEFT JOIN location l ON l.id=d.location \
             LEFT JOIN deferred_settlement s ON s.deferred=d.id WHERE "
        )
    };
}

fn deferred_row(row: &Row) -> Result<Deferred> {
    let subject = match row.get::<Option<i64>>(1)? {
        Some(component) => DeferredSubject::Component(ComponentId(component)),
        None => DeferredSubject::Path(PathId(row.get(2)?)),
    };
    Ok(Deferred {
        id: DeferredId(row.get(0)?),
        subject,
        reason: row.get(3)?,
        settled: row.get::<Option<String>>(4)?.map(|note| Settlement {
            note,
            guarantees: Vec::new(),
        }),
        location: match row.get::<Option<String>>(5)? {
            Some(_) => Some(scope::location_column(row, 5)?),
            None => None,
        },
    })
}

/// The deferred regions of `subject`.
pub(super) fn deferred_of(db: &Db, subject: DeferredSubject) -> Result<Vec<Deferred>> {
    let (sql, id): (&'static str, i64) = match subject {
        DeferredSubject::Component(id) => (
            concat!(deferred_select!(), "d.component=?1 ORDER BY d.id"),
            id.0,
        ),
        DeferredSubject::Path(id) => (concat!(deferred_select!(), "d.path=?1 ORDER BY d.id"), id.0),
    };
    let mut deferred = rows(db, sql, params![id], deferred_row)?;
    for region in &mut deferred {
        if let Some(settlement) = &mut region.settled {
            settlement.guarantees = rows(
                db,
                "SELECT guarantee FROM settlement_guarantee WHERE deferred=?1 ORDER BY guarantee",
                params![region.id.0],
                |row| Ok(GuaranteeId(row.get(0)?)),
            )?;
        }
    }
    Ok(deferred)
}

/// Guarantees, decoded by [`guarantee_row`]; a condition follows.
macro_rules! guarantee_select {
    () => {
        "SELECT g.id,g.component,g.kind,g.statement,g.preconditions,g.retry_status,g.function \
         FROM guarantee g WHERE "
    };
}

fn guarantee_row(row: &Row) -> Result<Guarantee> {
    Ok(Guarantee {
        id: GuaranteeId(row.get(0)?),
        component: ComponentId(row.get(1)?),
        kind: enum_column(row, 2)?,
        statement: row.get(3)?,
        preconditions: row.get(4)?,
        retry_status: row.get(5)?,
        function: row.get(6)?,
        evidence: Vec::new(),
        undermined_by: Vec::new(),
    })
}

fn hydrate_guarantee(db: &Db, mut guarantee: Guarantee) -> Result<Guarantee> {
    guarantee.evidence = flaws::evidence_of(db, EvidenceSubject::Guarantee(guarantee.id))?;
    guarantee.undermined_by = consequences::underminers(db, Invariant::Guarantee(guarantee.id))?;
    Ok(guarantee)
}

/// Whether a path with an untrusted source reaches the component in `$component`.
macro_rules! untrusted_reach {
    ($component:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM path_reach r JOIN path_source ps ON ps.path=r.path JOIN source \
             s ON s.id=ps.source JOIN trust_kind t ON t.name=s.trust WHERE r.component=",
            $component,
            " AND t.untrusted)"
        )
    };
}
pub(super) use untrusted_reach;

/// Whether the component in `$component` is reached by a path with an untrusted source and
/// the record's `$kind` is load-bearing.
macro_rules! load_bearing {
    ($component:literal, $kind:literal) => {
        concat!(
            "(SELECT load_bearing FROM invariant_kind WHERE name=",
            $kind,
            ") AND ",
            components::untrusted_reach!($component)
        )
    };
}

pub(super) use load_bearing;

fn dossier(db: &Db, id: ComponentId) -> Result<Dossier> {
    let component = component(db, id)?;
    let guarantees = rows(
        db,
        concat!(guarantee_select!(), "g.component=?1 ORDER BY g.id"),
        params![id.0],
        guarantee_row,
    )?
    .into_iter()
    .map(|guarantee| hydrate_guarantee(db, guarantee))
    .collect::<Result<_>>()?;
    Ok(Dossier {
        files: rows(
            db,
            "SELECT file FROM component_file WHERE component=?1 ORDER BY file",
            params![id.0],
            |row| Ok(row.get(0)?),
        )?,
        reached_by: rows(
            db,
            "SELECT path FROM path_reach WHERE component=?1 ORDER BY path",
            params![id.0],
            |row| Ok(PathId(row.get(0)?)),
        )?,
        sweeps: plan::sweeps_of(db, id)?,
        guarantees,
        checks: checks::of_component(db, id)?,
        retry_sites: retry::of(db, id)?,
        deferred: deferred_of(db, DeferredSubject::Component(id))?,
        flaws: flaws::of_component(db, id)?,
        component,
    })
}

impl Store {
    /// Records a component, or adds files to the one with this name; a file another
    /// component owns moves here unless a completed sweep already covered it. A reached
    /// component its sweeps no longer cover is planned a sweep.
    pub fn add_component(&self, actor: &AgentId, input: &NewComponent) -> Result<ComponentId> {
        self.write(|db| {
            db.execute(
                "INSERT INTO component(name,description) VALUES (?1,?2) ON CONFLICT(name) DO NOTHING",
                params![&input.name, &input.description],
            )?;
            let id = stored(
                db,
                "SELECT id FROM component WHERE name=?1",
                params![&input.name],
                |row| Ok(ComponentId(row.get(0)?)),
            )?;
            for file in &input.files {
                if let Some(owner) = optional(
                    db,
                    "SELECT cf.component FROM component_file cf WHERE cf.file=?1 AND cf.component!=?2 \
                     AND EXISTS (SELECT 1 FROM swept_file WHERE file=cf.file)",
                    params![file, id.0],
                    |row| Ok(ComponentId(row.get(0)?)),
                )? {
                    return Err(Error::FileSwept {
                        file: file.to_string(),
                        component: owner,
                    });
                }
                db.execute(
                    "INSERT INTO component_file(file,component,joined) VALUES (?1,?2,(SELECT \
                     coalesce(max(seq),0) FROM entry)) ON CONFLICT DO UPDATE SET \
                     component=excluded.component,joined=excluded.joined WHERE component IS NOT \
                     excluded.component",
                    params![file, id.0],
                )?;
            }
            plan::sweep_owed(db, actor, id)?;
            audit(db, actor, EventKind::Recorded, Detail::Record(EventRecord::Component(id)), &[])?;
            Ok(id)
        })
    }

    /// An invariant the component is argued to enforce, under its preconditions, with the
    /// evidence for the argument; a load-bearing one names the function it is argued in, and
    /// one spanning a retry status cites every site of it the component records.
    pub fn add_guarantee(
        &self,
        actor: &AgentId,
        component: ComponentId,
        guarantee: &NewGuarantee,
    ) -> Result<GuaranteeId> {
        if guarantee.kind.load_bearing() && guarantee.function.is_none() {
            return Err(Error::GuaranteeNeedsFunction {
                kind: guarantee.kind,
            });
        }
        self.write(|db| {
            scope::known(db, component)?;
            let id = GuaranteeId(db.insert(
                "INSERT INTO guarantee(component,function,kind,statement,preconditions,\
                 retry_status) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    component.0,
                    guarantee.function.as_ref(),
                    guarantee.kind,
                    &guarantee.statement,
                    &guarantee.preconditions,
                    guarantee.retry_status.as_ref()
                ],
            )?);
            flaws::insert_evidence(
                db,
                actor,
                EvidenceSubject::Guarantee(id),
                &guarantee.evidence,
            )?;
            if let Some(status) = &guarantee.retry_status {
                retry::cited(db, id, component, status.as_str())?;
            }
            audit(
                db,
                actor,
                EventKind::Recorded,
                Detail::Record(EventRecord::Guarantee(id)),
                &[],
            )?;
            Ok(id)
        })
    }

    /// A region of a component, or a stretch of a path, left unexamined, and why.
    pub fn defer(
        &self,
        actor: &AgentId,
        subject: DeferredSubject,
        location: Option<&Location>,
        reason: &Text,
    ) -> Result<DeferredId> {
        self.write(|db| {
            let (component, path) = match subject {
                DeferredSubject::Component(id) => {
                    scope::known(db, id)?;
                    (Some(id.0), None)
                }
                DeferredSubject::Path(id) => {
                    paths::revision(db, id)?;
                    (None, Some(id.0))
                }
            };
            let location = location
                .map(|location| scope::location_id(db, location))
                .transpose()?;
            let id = DeferredId(db.insert(
                "INSERT INTO deferred(component,path,location,reason) VALUES (?1,?2,?3,?4)",
                params![component, path, location.map(|id| id.0), reason],
            )?);
            audit(
                db,
                actor,
                EventKind::Recorded,
                Detail::Record(EventRecord::Deferred(id)),
                &[],
            )?;
            Ok(id)
        })
    }

    /// A later examination covered a deferred region, resting on `guarantees`, each with
    /// evidence inside the region.
    pub fn settle_deferred(
        &self,
        actor: &AgentId,
        deferred: DeferredId,
        guarantees: &[GuaranteeId],
        note: &Text,
    ) -> Result<()> {
        self.write(|db| {
            scope::known(db, deferred)?;
            if guarantees.len() > MAX_ATTACHMENTS {
                return Err(Error::Limit(Bound::Attachments));
            }
            for guarantee in guarantees {
                scope::known(db, *guarantee)?;
                if !exists(
                    db,
                    "SELECT 1 FROM deferred d JOIN location l ON l.id=d.location JOIN evidence e ON \
                     e.guarantee=?2 JOIN location el ON el.id=e.location WHERE d.id=?1 AND \
                     el.file=l.file AND el.start_line>=l.start_line AND el.end_line<=l.end_line",
                    params![deferred.0, guarantee.0],
                )? {
                    return Err(Error::OutsideDeferred {
                        guarantee: *guarantee,
                        deferred,
                    });
                }
            }
            let actor_id = actor_row(db, actor)?;
            db.execute(
                "INSERT INTO deferred_settlement(deferred,note,actor,at) VALUES (?1,?2,?3,?4) ON CONFLICT DO UPDATE SET note=excluded.note,actor=excluded.actor,at=excluded.at",
                params![deferred.0, note, actor_id, now()],
            )?;
            db.execute(
                "DELETE FROM settlement_guarantee WHERE deferred=?1",
                params![deferred.0],
            )?;
            for guarantee in guarantees {
                db.execute(
                    "INSERT INTO settlement_guarantee(deferred,guarantee) VALUES (?1,?2) ON \
                     CONFLICT DO NOTHING",
                    params![deferred.0, guarantee.0],
                )?;
            }
            audit(db, actor, EventKind::Updated, Detail::Record(EventRecord::Deferred(deferred)), &[])
                .map(drop)
        })
    }

    pub fn components(&self, page: Page) -> Result<Listing<Component>> {
        self.read(|db| {
            let components = rows(
                db,
                concat!(component_select!(), "c.id>?1 ORDER BY c.id LIMIT ?2"),
                params![page.after, page.limit()],
                component_row,
            )?;
            Ok(Listing::keyset(components, page))
        })
    }

    pub fn dossier(&self, id: ComponentId) -> Result<Dossier> {
        self.read(|db| dossier(db, id))
    }
}

#[cfg(test)]
mod tests {
    use skyhook::job::JobState;

    use super::{
        super::tests::{
            answerer, child_agent, component, evidence_at, fixture, location, new_guarantee, path,
            source, text,
        },
        *,
    };

    /// A sweep covers the files its component held when its agent started; one that joined
    /// during the sweep owes another, which alone answers for its retry sites, and a covered
    /// file stays with its component.
    #[test]
    fn a_file_moves_between_components_until_a_sweep_covers_it() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let refined = NewComponent {
            name: text("state"),
            description: text("the decoder's state machine"),
            files: vec![text("decoder/lib.c")],
        };
        let state = store.add_component(&actor, &refined).unwrap();
        assert!(store.dossier(decoder).unwrap().files.is_empty());
        assert_eq!(store.dossier(state).unwrap().files, ["decoder/lib.c"]);

        path(&store, &actor, "entry", &[state]);
        let sweeper = child_agent(&store, JobState::Running).0;
        let late = NewComponent {
            files: vec![text("decoder/tables.c")],
            ..refined
        };
        store.add_component(&actor, &late).unwrap();
        // A site in the late file waits for the sweep that covers it.
        let site = |class| NewRetrySite {
            location: location("decoder/tables.c", 7).in_function(text("Decode")),
            status: text("NEEDS_MORE_INPUT"),
            producer: None,
            class,
        };
        store.record_retry_site(&actor, &site(None)).unwrap();
        let sweeps = || store.dossier(state).unwrap().sweeps;
        let coverage = || store.dossier(state).unwrap().component.coverage;
        store
            .record_outcome(
                &sweeper,
                sweeps()[0].id,
                Outcome::Completed,
                &text("swept"),
                &[],
            )
            .unwrap();
        assert_eq!(coverage(), ComponentCoverage::Reached);
        let [_, again] = &sweeps()[..] else {
            panic!("the file that joined during the sweep owes another")
        };
        let complete = || {
            store.record_outcome(
                &answerer(&actor),
                again.id,
                Outcome::Completed,
                &text("swept"),
                &[],
            )
        };
        assert!(matches!(
            complete(),
            Err(Error::UnclassifiedRetrySites { .. })
        ));
        store
            .record_retry_site(&actor, &site(Some(RetryClass::Invalid)))
            .unwrap();
        complete().unwrap();
        assert_eq!(coverage(), ComponentCoverage::Inspected);
        let back = NewComponent {
            name: text("decoder"),
            description: text("fixture component"),
            files: vec![text("decoder/lib.c")],
        };
        assert!(matches!(
            store.add_component(&actor, &back),
            Err(Error::FileSwept { component, .. }) if component == state
        ));
    }
    /// A deferred region is settled on a guarantee only when the guarantee's evidence lies
    /// inside it; a region without a location has nothing inside it.
    #[test]
    fn a_deferral_is_settled_only_by_guarantees_argued_inside_it() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let guarantee = |line| {
            let evidence = vec![evidence_at("decoder/lib.c", line)];
            let guarantee = new_guarantee(InvariantKind::Bounds, evidence);
            store.add_guarantee(&actor, decoder, &guarantee).unwrap()
        };
        let (outside, inside) = (guarantee(50), guarantee(150));
        let region = Location::new(text("decoder/lib.c"), 100, 200, None).unwrap();
        let located = store
            .defer(
                &actor,
                DeferredSubject::Component(decoder),
                Some(&region),
                &text("command loop"),
            )
            .unwrap();
        let unlocated = store
            .defer(
                &actor,
                DeferredSubject::Component(decoder),
                None,
                &text("tables"),
            )
            .unwrap();
        let settle = |deferred, guarantees: &[GuaranteeId]| {
            store.settle_deferred(&actor, deferred, guarantees, &text("examined"))
        };
        assert!(matches!(
            settle(located, &[inside, outside]),
            Err(Error::OutsideDeferred { guarantee, .. }) if guarantee == outside
        ));
        assert!(matches!(
            settle(unlocated, &[inside]),
            Err(Error::OutsideDeferred { .. })
        ));
        settle(located, &[inside]).unwrap();
        let deferred = store.dossier(decoder).unwrap().deferred;
        assert_eq!(deferred[0].settled.as_ref().unwrap().guarantees, [inside]);
    }

    /// Load-bearing guarantees on untrusted paths stay unchallenged until a unit other than
    /// one ended deferred asks about them; components only trusted paths reach, and other
    /// kinds, are left alone.
    #[test]
    fn load_bearing_guarantees_are_challenged_where_untrusted_input_reaches() {
        let (_directory, store, actor) = fixture();
        let exposed = component(&store, &actor, "decoder");
        let internal = component(&store, &actor, "admin");
        path(&store, &actor, "stream", &[exposed]);
        let operator = source(&store, &actor, "operator", Trust::Privileged);
        store
            .add_path(
                &actor,
                &NewPath {
                    description: text("operator"),
                    sources: vec![operator],
                    reaches: vec![Reach {
                        component: internal,
                        note: text("direct call"),
                    }],
                },
            )
            .unwrap();
        let guarantee = |component, kind| {
            (store.add_guarantee(&actor, component, &new_guarantee(kind, Vec::new()))).unwrap()
        };
        let bounded = guarantee(exposed, InvariantKind::Bounds);
        guarantee(exposed, InvariantKind::Authorization);
        guarantee(internal, InvariantKind::Bounds);
        let unchallenged = || store.coverage().unwrap().unchallenged;
        assert_eq!(unchallenged(), 1);
        let question = store
            .plan_unit(
                &actor,
                &UnitWork::Question {
                    question: text("can a retry stage more than one attempt?"),
                    stopping: text("a counterexample or a proof over every return site"),
                    invariants: vec![Invariant::Guarantee(bounded)],
                    check: None,
                },
            )
            .unwrap();
        assert_eq!(unchallenged(), 0);
        store
            .record_outcome(
                &actor,
                question,
                Outcome::Deferred,
                &text("unchallenged"),
                &[],
            )
            .unwrap();
        assert_eq!(unchallenged(), 1);
    }
}
