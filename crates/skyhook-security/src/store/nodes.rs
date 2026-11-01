//! Nodes created and edited in place, and their statuses. An edit changing anything but the
//! title is a material change of the node; a title-only or restating edit only concerns it.

use std::{collections::BTreeSet, num::NonZeroU32};

use schemars::JsonSchema;
use serde::Deserialize;
use skyhook::db::{Db, Sql as _, enum_column, params};

use super::*;

/// A node's text when it is created, with the fields of its kind.
#[derive(Clone, Debug)]
pub struct New<D> {
    pub title: Text,
    pub body: Text,
    pub detail: D,
}

/// The fields an edit gives; the rest stay as they are.
#[derive(Clone, Debug)]
pub struct Edit<D> {
    pub title: Option<Text>,
    pub body: Option<Text>,
    pub detail: D,
}

/// A node created, or the node `I` edited.
#[derive(Clone, Debug)]
pub enum Change<I, N, E> {
    Create(New<N>),
    Edit(I, Edit<E>),
}

/// A component and the files and interfaces it owns; one named again moves to it.
#[derive(Clone, Debug)]
pub struct NewComponent {
    pub files: Vec<Text>,
    pub interfaces: Vec<InterfaceName>,
}
#[derive(Clone, Debug, Default)]
pub struct ComponentEdit {
    pub files: Vec<Text>,
    pub interfaces: Vec<InterfaceName>,
}

#[derive(Clone, Debug)]
pub struct NewSource {
    pub location: Location,
    pub trust: Trust,
}
#[derive(Clone, Debug, Default)]
pub struct SourceEdit {
    pub location: Option<Location>,
    pub trust: Option<Trust>,
}

#[derive(Clone, Debug)]
pub struct NewPremise {
    pub scope: PremiseScope,
    pub location: Option<Location>,
}
#[derive(Clone, Debug, Default)]
pub struct PremiseEdit {
    pub scope: Option<PremiseScope>,
    pub location: Option<Location>,
}

#[derive(Clone, Debug)]
pub struct NewFlaw {
    pub component: ComponentId,
    pub location: Location,
    pub cwe: Option<NonZeroU32>,
}
#[derive(Clone, Debug, Default)]
pub struct FlawEdit {
    pub component: Option<ComponentId>,
    pub location: Option<Location>,
    pub cwe: Option<NonZeroU32>,
}

/// A candidate performing `actions`, chaining `chain` in order from its root.
#[derive(Clone, Debug)]
pub struct NewCandidate {
    pub actions: Actions,
    pub chain: Chain,
}
/// A candidate's edit: its whole list of actions, or its whole chain, in order.
#[derive(Clone, Debug, Default)]
pub struct CandidateEdit {
    pub actions: Option<Actions>,
    pub chain: Option<Chain>,
}

/// At least one node of `I`'s kind, in order, as a candidate lists them.
#[derive(Clone, Debug)]
pub struct Sequence<I>(Vec<I>);

/// A candidate's flaws, its root first.
pub type Chain = Sequence<FlawId>;
/// The paths a candidate's scenario performs, the first where the attacker starts.
pub type Actions = Sequence<PathId>;

impl<I: KindId> TryFrom<Vec<I>> for Sequence<I> {
    type Error = Error;
    fn try_from(ids: Vec<I>) -> Result<Self> {
        match ids.len() {
            0 => Err(Error::Empty(I::KIND)),
            MAX_ATTACHMENTS.. => Err(Error::Limit(Bound::Attachments)),
            _ => Ok(Self(ids)),
        }
    }
}

/// A node created or edited, its kind's fields typed.
#[derive(Clone, Debug)]
pub enum Upsert {
    Component(Change<ComponentId, NewComponent, ComponentEdit>),
    Path(Change<PathId, (), ()>),
    Source(Change<SourceId, NewSource, SourceEdit>),
    Premise(Change<PremiseId, NewPremise, PremiseEdit>),
    Flaw(Change<FlawId, NewFlaw, FlawEdit>),
    Candidate(Change<CandidateId, NewCandidate, CandidateEdit>),
}

/// A node as a request names it: with an `id` it is an edit of the fields given, without one a
/// new node, which needs the fields of its kind.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UpsertInput {
    Component {
        id: Option<ComponentId>,
        title: Option<Text>,
        body: Option<Text>,
        #[serde(default)]
        files: Vec<Text>,
        #[serde(default)]
        interfaces: Vec<InterfaceName>,
    },
    Path {
        id: Option<PathId>,
        title: Option<Text>,
        body: Option<Text>,
    },
    Source {
        id: Option<SourceId>,
        title: Option<Text>,
        body: Option<Text>,
        location: Option<Location>,
        trust: Option<Trust>,
    },
    Premise {
        id: Option<PremiseId>,
        title: Option<Text>,
        body: Option<Text>,
        scope: Option<PremiseScope>,
        location: Option<Location>,
    },
    Flaw {
        id: Option<FlawId>,
        title: Option<Text>,
        body: Option<Text>,
        component: Option<ComponentId>,
        location: Option<Location>,
        cwe: Option<NonZeroU32>,
    },
    Candidate {
        id: Option<CandidateId>,
        title: Option<Text>,
        body: Option<Text>,
        /// The paths its scenario performs, in order, the first where the attacker starts.
        paths: Option<Vec<PathId>>,
        flaws: Option<Vec<FlawId>>,
    },
}

/// The fields of `named` that are absent.
fn missing<const N: usize>(kind: NodeKind, named: [(&'static str, bool); N]) -> Error {
    let fields = (named.into_iter())
        .filter(|(_, present)| !present)
        .map(|(name, _)| name)
        .collect();
    Error::Missing {
        kind,
        fields: Fields(fields),
    }
}

/// Each kind's input as its change: with an `id` an edit of the fields given, without one a
/// create needing title, body and the kind's `[required]` fields; `[optional]` fields pass to
/// both. `$new` and `$edit` build the kind's detail from them.
macro_rules! upserts {
    ($input:expr, $(
        $kind:ident [$($required:ident),*] [$($optional:ident),*] => $new:expr, $edit:expr;
    )+) => {
        match $input {
            $(
                UpsertInput::$kind { id: Some(id), title, body, $($required,)* $($optional,)* } => {
                    Upsert::$kind(Change::Edit(id, Edit { title, body, detail: $edit }))
                }
                UpsertInput::$kind { id: None, title, body, $($required,)* $($optional,)* } => {
                    match (title, body, $($required),*) {
                        (Some(title), Some(body), $(Some($required)),*) => {
                            Upsert::$kind(Change::Create(New { title, body, detail: $new }))
                        }
                        (title, body, $($required),*) => {
                            let named = [
                                ("title", title.is_some()),
                                ("body", body.is_some()),
                                $((stringify!($required), $required.is_some())),*
                            ];
                            return Err(missing(NodeKind::$kind, named));
                        }
                    }
                }
            )+
        }
    };
}

impl TryFrom<UpsertInput> for Upsert {
    type Error = Error;
    fn try_from(input: UpsertInput) -> Result<Self> {
        Ok(upserts!(input,
            Component [] [files, interfaces] =>
                NewComponent { files, interfaces }, ComponentEdit { files, interfaces };
            Path [] [] => (), ();
            Source [location, trust] [] =>
                NewSource { location, trust }, SourceEdit { location, trust };
            Premise [scope] [location] =>
                NewPremise { scope, location }, PremiseEdit { scope, location };
            Flaw [component, location] [cwe] =>
                NewFlaw { component, location, cwe }, FlawEdit { component, location, cwe };
            Candidate [paths, flaws] [] =>
                NewCandidate { actions: paths.try_into()?, chain: flaws.try_into()? },
                CandidateEdit {
                    actions: paths.map(Actions::try_from).transpose()?,
                    chain: flaws.map(Chain::try_from).transpose()?,
                };
        ))
    }
}

/// Run `sql`, an update of node `?1` to `?2` that matches only when the value differs: whether
/// it changed.
fn set(db: &Db, sql: &'static str, node: NodeId, value: libsql::Value) -> Result<bool> {
    Ok(db.execute(sql, vec![node.sql(), value])? > 0)
}

/// What a component owns by name, each name owned by one component: its files and its
/// interfaces.
struct Owned {
    owner: &'static str,
    own: &'static str,
}

macro_rules! owned {
    ($table:literal, $column:literal) => {
        Owned {
            owner: concat!("SELECT component FROM ", $table, " WHERE ", $column, "=?1"),
            own: concat!(
                "INSERT INTO ",
                $table,
                "(",
                $column,
                ",component) VALUES (?1,?2) ON CONFLICT(",
                $column,
                ") DO UPDATE SET component=excluded.component"
            ),
        }
    };
}

const FILES: Owned = owned!("component_file", "file");
const INTERFACES: Owned = owned!("component_interface", "interface");

impl Owned {
    /// `names` owned by `component`; each one owned elsewhere before is marked changed with
    /// its owner: whether any was new to it.
    fn own(
        &self,
        db: &Db,
        component: NodeId,
        names: &[impl std::borrow::Borrow<str>],
        marks: &mut Marks,
    ) -> Result<bool> {
        if names.len() > MAX_ATTACHMENTS {
            return Err(Error::Limit(Bound::Attachments));
        }
        let mut changed = false;
        for name in names {
            let name: &str = name.borrow();
            let owner = optional(db, self.owner, params![name], |row| Ok(NodeId(row.get(0)?)))?;
            if owner == Some(component) {
                continue;
            }
            db.execute(self.own, params![name, component])?;
            if let Some(owner) = owner {
                marks.mark(owner, EventEffect::Changed);
            }
            changed = true;
        }
        Ok(changed)
    }
}

/// The kind fields of a new node: the live node already recorded with them, which a create
/// returns instead, or else insert its detail row and anything it holds, marking the other nodes
/// it bears on.
trait Detail {
    const KIND: NodeKind;
    fn recorded(&self, _: &Db) -> Result<Option<NodeId>> {
        Ok(None)
    }
    fn create(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<()>;
}

/// The kind fields an edit gives: apply them, marking other nodes it bears on; whether any
/// changed.
trait DetailEdit {
    fn edit(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<bool>;
}

impl Detail for NewComponent {
    const KIND: NodeKind = NodeKind::Component;
    fn create(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<()> {
        db.execute("INSERT INTO component(node) VALUES (?1)", params![node])?;
        FILES.own(db, node, &self.files, marks)?;
        INTERFACES.own(db, node, &self.interfaces, marks)?;
        Ok(())
    }
}
impl DetailEdit for ComponentEdit {
    fn edit(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<bool> {
        Ok(FILES.own(db, node, &self.files, marks)?
            | INTERFACES.own(db, node, &self.interfaces, marks)?)
    }
}

impl Detail for () {
    const KIND: NodeKind = NodeKind::Path;
    fn create(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<()> {
        db.execute("INSERT INTO path(node) VALUES (?1)", params![node])?;
        Ok(())
    }
}
impl DetailEdit for () {
    fn edit(&self, _: &Db, _: NodeId, _: &mut Marks) -> Result<bool> {
        Ok(false)
    }
}

/// The source recorded at `location`: only a live source holds one.
fn source_at(db: &Db, location: LocationId) -> Result<Option<SourceId>> {
    optional(
        db,
        "SELECT node FROM source WHERE location=?1",
        params![location],
        |row| Ok(SourceId(row.get(0)?)),
    )
}

impl Detail for NewSource {
    const KIND: NodeKind = NodeKind::Source;
    fn recorded(&self, db: &Db) -> Result<Option<NodeId>> {
        Ok(source_at(db, location_id(db, &self.location)?)?.map(NodeId::from))
    }
    fn create(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<()> {
        db.execute(
            "INSERT INTO source(node,location,trust) VALUES (?1,?2,?3)",
            params![node, location_id(db, &self.location)?, self.trust],
        )?;
        Ok(())
    }
}
impl DetailEdit for SourceEdit {
    fn edit(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<bool> {
        let mut changed = false;
        if let Some(location) = &self.location {
            let location = location_id(db, location)?;
            match source_at(db, location)? {
                Some(source) if NodeId::from(source) != node => {
                    return Err(Error::SourceAt(source));
                }
                _ => {}
            }
            changed |= set(
                db,
                "UPDATE source SET location=?2 WHERE node=?1 AND location!=?2",
                node,
                location.sql(),
            )?;
        }
        if let Some(trust) = self.trust {
            changed |= set(
                db,
                "UPDATE source SET trust=?2 WHERE node=?1 AND trust!=?2",
                node,
                trust.sql(),
            )?;
        }
        Ok(changed)
    }
}

impl Detail for NewPremise {
    const KIND: NodeKind = NodeKind::Premise;
    fn create(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<()> {
        let location = self
            .location
            .as_ref()
            .map(|at| location_id(db, at))
            .transpose()?;
        db.execute(
            "INSERT INTO premise(node,scope,location) VALUES (?1,?2,?3)",
            params![node, self.scope, location],
        )?;
        Ok(())
    }
}
impl DetailEdit for PremiseEdit {
    fn edit(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<bool> {
        let mut changed = false;
        if let Some(scope) = self.scope {
            changed |= set(
                db,
                "UPDATE premise SET scope=?2 WHERE node=?1 AND scope!=?2",
                node,
                scope.sql(),
            )?;
        }
        if let Some(location) = &self.location {
            changed |= set(
                db,
                "UPDATE premise SET location=?2 WHERE node=?1 AND location IS NOT ?2",
                node,
                location_id(db, location)?.sql(),
            )?;
        }
        Ok(changed)
    }
}

impl Detail for NewFlaw {
    const KIND: NodeKind = NodeKind::Flaw;
    fn create(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<()> {
        db.execute(
            "INSERT INTO flaw(node,component,location,cwe) VALUES (?1,?2,?3,?4)",
            params![
                node,
                live(db, self.component)?,
                location_id(db, &self.location)?,
                self.cwe.map(NonZeroU32::get)
            ],
        )?;
        Ok(())
    }
}
impl DetailEdit for FlawEdit {
    fn edit(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<bool> {
        let mut changed = false;
        if let Some(component) = self.component {
            changed |= set(
                db,
                "UPDATE flaw SET component=?2 WHERE node=?1 AND component!=?2",
                node,
                live(db, component)?.sql(),
            )?;
        }
        if let Some(location) = &self.location {
            changed |= set(
                db,
                "UPDATE flaw SET location=?2 WHERE node=?1 AND location!=?2",
                node,
                location_id(db, location)?.sql(),
            )?;
        }
        if let Some(cwe) = self.cwe {
            changed |= set(
                db,
                "UPDATE flaw SET cwe=?2 WHERE node=?1 AND cwe IS NOT ?2",
                node,
                cwe.get().sql(),
            )?;
        }
        Ok(changed)
    }
}

/// The statements reading and replacing one of a candidate `?1`'s ordered lists: clearing
/// it from position `?2` on.
pub(super) struct Listed {
    held: &'static str,
    clear: &'static str,
    insert: &'static str,
}

macro_rules! listed {
    ($table:literal, $column:literal) => {
        Listed {
            held: concat!(
                "SELECT ",
                $column,
                " FROM ",
                $table,
                " WHERE candidate=?1 ORDER BY position"
            ),
            clear: concat!(
                "DELETE FROM ",
                $table,
                " WHERE candidate=?1 AND position>=?2"
            ),
            insert: concat!(
                "INSERT INTO ",
                $table,
                "(candidate,position,",
                $column,
                ") VALUES (?1,?2,?3)"
            ),
        }
    };
}

pub(super) const FLAWS: Listed = listed!("candidate_flaw", "flaw");
pub(super) const ACTIONS: Listed = listed!("candidate_action", "path");

impl Listed {
    /// Candidate `node`'s list, in order.
    pub(super) fn held<I: KindId>(&self, db: &Db, node: impl Into<NodeId>) -> Result<Vec<I>> {
        rows(db, self.held, params![node.into()], |row| {
            Ok(I::of(NodeId(row.get(0)?)))
        })
    }

    /// Give candidate `node` the list `wanted` in place of the one it has, rewriting it past
    /// the prefix they share, each node joining or leaving it concerned: the list it held,
    /// unless that was `wanted` already.
    pub(super) fn set<I: KindId + PartialEq>(
        &self,
        db: &Db,
        node: impl Into<NodeId>,
        wanted: &[I],
        marks: &mut Marks,
    ) -> Result<Option<Vec<I>>> {
        let node = node.into();
        let held = self.held(db, node)?;
        if held == wanted {
            return Ok(None);
        }
        let kept = (held.iter().zip(wanted))
            .take_while(|(held, wanted)| held == wanted)
            .count();
        db.execute(self.clear, params![node, kept])?;
        for (position, id) in wanted.iter().enumerate().skip(kept) {
            db.execute(
                self.insert,
                params![node, position, Into::<NodeId>::into(*id)],
            )?;
        }
        let members =
            |list: &[I]| -> BTreeSet<NodeId> { list.iter().map(|id| (*id).into()).collect() };
        for id in members(&held).symmetric_difference(&members(wanted)) {
            marks.mark(*id, EventEffect::Concerns);
        }
        Ok(Some(held))
    }
}

/// Chain `chain`'s live flaws in order to candidate `node`, in place of the chain it has: each
/// flaw joining or leaving it is concerned, and one taking or losing its root role moved;
/// whether the chain changed.
fn set_chain(db: &Db, node: NodeId, chain: &Chain, marks: &mut Marks) -> Result<bool> {
    let (mut wanted, mut seen) = (Vec::with_capacity(chain.0.len()), BTreeSet::new());
    for flaw in &chain.0 {
        let flaw = live(db, *flaw)?;
        if seen.insert(NodeId::from(flaw)) {
            wanted.push(flaw);
        }
    }
    let Some(held) = FLAWS.set(db, node, &wanted, marks)? else {
        return Ok(false);
    };
    marks.rerooted(held.first().copied(), wanted.first().copied());
    Ok(true)
}

/// Give candidate `node` `actions`' live paths in order, in place of the ones it performs; each
/// path joining or leaving it is concerned: whether they changed.
fn set_actions(db: &Db, node: NodeId, actions: &Actions, marks: &mut Marks) -> Result<bool> {
    let wanted = (actions.0.iter())
        .map(|path| live(db, *path))
        .collect::<Result<Vec<_>>>()?;
    Ok(ACTIONS.set(db, node, &wanted, marks)?.is_some())
}

impl Detail for NewCandidate {
    const KIND: NodeKind = NodeKind::Candidate;
    fn create(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<()> {
        db.execute("INSERT INTO candidate(node) VALUES (?1)", params![node])?;
        set_actions(db, node, &self.actions, marks)?;
        set_chain(db, node, &self.chain, marks)?;
        Ok(())
    }
}
impl DetailEdit for CandidateEdit {
    fn edit(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<bool> {
        let mut changed = false;
        if let Some(actions) = &self.actions {
            changed |= set_actions(db, node, actions, marks)?;
        }
        if let Some(chain) = &self.chain {
            changed |= set_chain(db, node, chain, marks)?;
        }
        Ok(changed)
    }
}

fn create<D: Detail>(db: &Db, actor: &AgentId, new: &New<D>) -> Result<NodeId> {
    if let Some(node) = new.detail.recorded(db)? {
        return Ok(node);
    }
    audit(db, actor, EventKind::Created, None, |_| {
        let node = NodeId(db.insert(
            "INSERT INTO node(kind,title,body) VALUES (?1,?2,?3)",
            params![D::KIND, &new.title, &new.body],
        )?);
        let mut marks = Marks::of(node, EventEffect::Changed);
        new.detail.create(db, node, &mut marks)?;
        Ok((node, marks))
    })
}

fn edit<I: KindId, D: DetailEdit>(
    db: &Db,
    actor: &AgentId,
    id: I,
    edit: &Edit<D>,
) -> Result<NodeId> {
    let node: NodeId = live(db, id)?.into();
    audit(db, actor, EventKind::Edited, None, |_| {
        if let Some(title) = &edit.title {
            set(
                db,
                "UPDATE node SET title=?2 WHERE id=?1 AND title!=?2",
                node,
                title.sql(),
            )?;
        }
        let mut marks = Marks::default();
        let mut changed = edit.detail.edit(db, node, &mut marks)?;
        if let Some(body) = &edit.body {
            changed |= set(
                db,
                "UPDATE node SET body=?2 WHERE id=?1 AND body!=?2",
                node,
                body.sql(),
            )?;
        }
        marks.mark(
            node,
            match changed {
                true => EventEffect::Changed,
                false => EventEffect::Concerns,
            },
        );
        Ok((node, marks))
    })
}

fn apply<I: KindId, N: Detail, E: DetailEdit>(
    db: &Db,
    actor: &AgentId,
    change: &Change<I, N, E>,
) -> Result<NodeId> {
    match change {
        Change::Create(new) => create(db, actor, new),
        Change::Edit(id, change) => edit(db, actor, *id, change),
    }
}

/// A status set: a premise decided, a flaw judged, a path marked swept or unswept, or a source,
/// path or component retracted or restored.
#[derive(Clone, Copy, Debug)]
pub enum StatusChange {
    Premise(PremiseId, PremiseStatus),
    Flaw(FlawId, FlawStatus),
    Path(PathId, PathStatus),
    Source(SourceId, InventoryStatus),
    Component(ComponentId, InventoryStatus),
}

/// A status as a request sets it, with the actor's reason.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum StatusInput {
    Premise {
        premise: PremiseId,
        status: PremiseStatus,
        note: Text,
    },
    Flaw {
        flaw: FlawId,
        status: FlawStatus,
        note: Text,
    },
    Path {
        path: PathId,
        status: PathStatus,
        note: Text,
    },
    Source {
        source: SourceId,
        status: InventoryStatus,
        note: Text,
    },
    Component {
        component: ComponentId,
        status: InventoryStatus,
        note: Text,
    },
}

impl StatusInput {
    pub fn change(&self) -> (StatusChange, &Text) {
        match self {
            Self::Premise {
                premise,
                status,
                note,
            } => (StatusChange::Premise(*premise, *status), note),
            Self::Flaw { flaw, status, note } => (StatusChange::Flaw(*flaw, *status), note),
            Self::Path { path, status, note } => (StatusChange::Path(*path, *status), note),
            Self::Source {
                source,
                status,
                note,
            } => (StatusChange::Source(*source, *status), note),
            Self::Component {
                component,
                status,
                note,
            } => (StatusChange::Component(*component, *status), note),
        }
    }
}

impl Store {
    /// Create a node or edit one, returning its id: a merged node's edit goes to the node it
    /// was merged into, and a source at a location already recorded is the source there.
    pub fn upsert(&self, actor: &AgentId, upsert: &Upsert) -> Result<NodeId> {
        self.write(|db| match upsert {
            Upsert::Component(change) => apply(db, actor, change),
            Upsert::Path(change) => apply(db, actor, change),
            Upsert::Source(change) => apply(db, actor, change),
            Upsert::Premise(change) => apply(db, actor, change),
            Upsert::Flaw(change) => apply(db, actor, change),
            Upsert::Candidate(change) => apply(db, actor, change),
        })
    }

    /// Set a status, with the actor's reason. A premise's or flaw's new value changes it, and
    /// restating the status it reads with only concerns it; either stamps it as decided or
    /// judged by this event. Marking a path, source or component moves its own standing alone,
    /// and a source's that of the paths it enters, so an unswept or restored path is offered
    /// its sweep again. A path retracted or restored also moves the standing of the candidates
    /// performing it, whose chains gain or lose the premises it relies on.
    pub fn set_status(&self, actor: &AgentId, change: StatusChange, note: &Text) -> Result<()> {
        self.write(|db| {
            audit(db, actor, EventKind::Status, Some(note), |event| {
                let mut marks = Marks::default();
                let decided = |changed| match changed {
                    true => EventEffect::Changed,
                    false => EventEffect::Concerns,
                };
                match change {
                    StatusChange::Premise(id, status) => {
                        let node = live(db, id)?;
                        let sql = "SELECT 1 FROM premise_state WHERE node=?1 AND status!=?2";
                        let changed = exists(db, sql, params![node, status])?;
                        let sql = "UPDATE premise SET status=?2,decided=?3 WHERE node=?1";
                        db.execute(sql, params![node, status, event])?;
                        marks.mark(node, decided(changed));
                    }
                    StatusChange::Flaw(id, status) => {
                        let node = live(db, id)?;
                        let sql = "SELECT 1 FROM flaw_state WHERE node=?1 AND status!=?2";
                        let changed = exists(db, sql, params![node, status])?;
                        let sql = "UPDATE flaw SET status=?2,judged=?3 WHERE node=?1";
                        db.execute(sql, params![node, status, event])?;
                        marks.mark(node, decided(changed));
                    }
                    StatusChange::Path(id, status) => {
                        let node = live(db, id)?;
                        let sql = "SELECT status FROM path WHERE node=?1";
                        let held: PathStatus =
                            stored(db, sql, params![node], |row| Ok(enum_column(row, 0)?))?;
                        let sql = "UPDATE path SET status=?2 WHERE node=?1";
                        db.execute(sql, params![node, status])?;
                        let retracted = PathStatus::Retracted;
                        match (held == retracted) != (status == retracted) {
                            true => marks.mark(node, EventEffect::Standing),
                            false => marks.alone(node),
                        }
                    }
                    StatusChange::Source(id, status) => {
                        let node = live(db, id)?;
                        let sql = "UPDATE source SET status=?2 WHERE node=?1";
                        db.execute(sql, params![node, status])?;
                        let sql = "SELECT path FROM path_source WHERE source=?1";
                        for path in rows(db, sql, params![node], |row| Ok(NodeId(row.get(0)?)))? {
                            marks.alone(path);
                        }
                        marks.alone(node);
                    }
                    StatusChange::Component(id, status) => {
                        let node = live(db, id)?;
                        let sql = "UPDATE component SET status=?2 WHERE node=?1";
                        db.execute(sql, params![node, status])?;
                        marks.alone(node);
                    }
                }
                Ok(((), marks))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use skyhook::job::JobState;

    use super::{
        super::tests::{
            HIGH, assess, child_agent, fixture, last_event, marks, premise, scenario, text, verdict,
        },
        *,
    };

    fn parse(value: serde_json::Value) -> Result<Upsert> {
        serde_json::from_value::<UpsertInput>(value)
            .unwrap()
            .try_into()
    }

    /// Each kind is created with its fields and edited in place, a candidate's chain included:
    /// an edit of anything but the title changes the node, while a title or a restatement only
    /// concerns it. A create
    /// lacking its kind's fields is refused naming them, and a detail row holds only a node of
    /// its kind.
    #[test]
    fn nodes_are_created_and_edited_in_place() {
        let (_session, store, actor) = fixture();
        let upsert = |value| store.upsert(&actor, &parse(value).unwrap()).unwrap();
        let at = json!({"code": {"file": "inflate.c", "start_line": 3, "end_line": 4}});
        let decoder = upsert(
            json!({"kind": "component", "title": "decoder", "body": "inflate",
            "files": ["inflate.c"], "interfaces": ["inflate-api"]}),
        );
        let tables = upsert(
            json!({"kind": "component", "title": "tables", "body": "huffman",
            "files": ["tables.c"]}),
        );
        let source = upsert(json!({"kind": "source", "title": "socket", "body": "reads",
            "location": at, "trust": "untrusted"}));
        let recorded = last_event(&store);
        assert_eq!(
            upsert(json!({"kind": "source", "title": "again", "body": "reads",
                "location": at, "trust": "local"})),
            source,
            "a source at a recorded location is the source there"
        );
        assert_eq!(last_event(&store), recorded);
        let elsewhere = upsert(json!({"kind": "source", "title": "pipe", "body": "reads",
            "location": {"code": {"file": "pipe.c", "start_line": 1, "end_line": 1}},
            "trust": "local"}));
        let moved = parse(json!({"kind": "source", "id": elsewhere, "location": at})).unwrap();
        assert!(matches!(
            store.upsert(&actor, &moved),
            Err(Error::SourceAt(at)) if NodeId::from(at) == source
        ));
        let path = upsert(json!({"kind": "path", "title": "stream", "body": "socket to inflate"}));
        let premise = upsert(
            json!({"kind": "premise", "title": "len checked", "body": "before \
            the copy", "scope": "code"}),
        );
        let flaw = upsert(json!({"kind": "flaw", "title": "copy", "body": "unbounded",
            "component": decoder, "location": at, "cwe": 120}));
        let candidate = upsert(
            json!({"kind": "candidate", "title": "overflow", "body": "heap",
            "paths": [path], "flaws": [flaw]}),
        );
        let kinds = [decoder, source, path, premise, flaw, candidate].map(|id| {
            match store.node(id).unwrap() {
                NodeView::Component(_) => NodeKind::Component,
                NodeView::Source(_) => NodeKind::Source,
                NodeView::Path(_) => NodeKind::Path,
                NodeView::Premise(_) => NodeKind::Premise,
                NodeView::Flaw(_) => NodeKind::Flaw,
                NodeView::Candidate(_) => NodeKind::Candidate,
                NodeView::Finding(_) => NodeKind::Finding,
            }
        });
        assert_eq!(
            kinds,
            [
                NodeKind::Component,
                NodeKind::Source,
                NodeKind::Path,
                NodeKind::Premise,
                NodeKind::Flaw,
                NodeKind::Candidate
            ]
        );
        assert!(matches!(
            parse(json!({"kind": "flaw", "title": "copy"})),
            Err(Error::Missing { kind: NodeKind::Flaw, fields }) if fields.0 == ["body", "component", "location"]
        ));
        let empty = |paths, flaws| {
            parse(
                json!({"kind": "candidate", "title": "t", "body": "b", "paths": paths,
                "flaws": flaws}),
            )
        };
        assert!(matches!(
            empty(json!([path]), json!([])),
            Err(Error::Empty(NodeKind::Flaw))
        ));
        assert!(matches!(
            empty(json!([]), json!([flaw])),
            Err(Error::Empty(NodeKind::Path))
        ));

        let effects = || marks(&store, last_event(&store));
        let concerns = |node| vec![(node, EventEffect::Concerns)];
        upsert(json!({"kind": "flaw", "id": flaw, "title": "unbounded copy"}));
        assert_eq!(effects(), concerns(flaw));
        upsert(
            json!({"kind": "flaw", "id": flaw, "component": decoder, "cwe": 120, "location": at}),
        );
        assert_eq!(effects(), concerns(flaw));
        upsert(json!({"kind": "flaw", "id": flaw, "cwe": 787}));
        assert_eq!(
            effects(),
            [
                (flaw, EventEffect::Changed),
                (candidate, EventEffect::Changed)
            ]
        );
        for owned in [
            json!({"files": ["inflate.c"]}),
            json!({"interfaces": ["inflate-api"]}),
        ] {
            let mut edit = json!({"kind": "component", "id": tables});
            edit.as_object_mut()
                .unwrap()
                .extend(owned.as_object().unwrap().clone());
            upsert(edit);
            assert_eq!(
                effects(),
                [
                    (decoder, EventEffect::Changed),
                    (tables, EventEffect::Changed)
                ],
                "a file or an interface named again moves, changing both owners"
            );
        }
        let other = upsert(json!({"kind": "flaw", "title": "copy", "body": "again",
            "component": decoder, "location": at}));
        upsert(json!({"kind": "candidate", "id": candidate, "flaws": [other, flaw]}));
        assert_eq!(
            effects(),
            [
                (flaw, EventEffect::Standing),
                (candidate, EventEffect::Changed),
                (other, EventEffect::Standing)
            ],
            "a chain is edited whole; a flaw joining it and the root it displaces are marked"
        );
        let NodeView::Flaw(view) = store.node(flaw).unwrap() else {
            panic!("a flaw")
        };
        assert_eq!(
            (view.head.title.as_str(), view.cwe),
            ("unbounded copy", Some(787))
        );
        let NodeView::Component(view) = store.node(tables).unwrap() else {
            panic!("a component")
        };
        assert_eq!(view.files, ["inflate.c", "tables.c"]);
        assert_eq!(view.interfaces, ["inflate-api".parse().unwrap()]);

        let misplaced = store.write(|db| {
            db.execute(
                "INSERT INTO premise(node,scope) VALUES (?1,'code')",
                params![flaw],
            )?;
            Ok(())
        });
        assert!(
            misplaced.is_err(),
            "a premise row holds only a premise node"
        );
    }

    /// Any agent marks a path swept, retracted or unswept: its view and listing show the
    /// status. Marking it swept or unswept moves the path alone; retracting or restoring it
    /// also moves the candidates performing it.
    #[test]
    fn any_agent_marks_a_paths_status() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let (path, candidate) = (scenario.path, scenario.candidate);
        let (stranger, _) = child_agent(&store, JobState::Running);
        let alone = vec![(path.into(), EventEffect::Standing)];
        let carried = vec![
            (path.into(), EventEffect::Standing),
            (candidate.into(), EventEffect::Standing),
        ];
        for (status, moved) in [
            (PathStatus::Swept, alone),
            (PathStatus::Retracted, carried.clone()),
            (PathStatus::Unswept, carried),
        ] {
            let change = StatusChange::Path(path, status);
            store
                .set_status(&stranger, change, &text("marked"))
                .unwrap();
            let NodeView::Path(view) = store.node(path.into()).unwrap() else {
                panic!("a path")
            };
            let listed = store.list(&ListQuery::Path, Page::default()).unwrap();
            assert_eq!(
                (view.status, listed[0].status),
                (status, Some(NodeStatus::Path(status)))
            );
            assert_eq!(marks(&store, last_event(&store)), moved);
        }
    }

    /// Restoring a retracted path brings the premises it relies on back into its candidates'
    /// chains, so a verdict and report made while it was retracted no longer stand.
    #[test]
    fn a_restored_path_reowes_its_candidates_verdict_and_report() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let held = premise(&store, &actor, PremiseScope::Code);
        let relies = Link::PathPremise {
            path: scenario.path,
            premise: held,
        };
        store.link(&actor, &[relies]).unwrap();
        let mark = |status| {
            let change = StatusChange::Path(scenario.path, status);
            store.set_status(&actor, change, &text("marked")).unwrap();
        };
        mark(PathStatus::Retracted);
        assess(&store, &actor, scenario.candidate, HIGH);
        verdict(&store, &actor, scenario.candidate, Verdict::Validated);
        let finding = store
            .report(&actor, scenario.flaw, &text("overflow"), &text("report"))
            .unwrap();
        let current = || {
            let NodeView::Candidate(candidate) = store.node(scenario.candidate.into()).unwrap()
            else {
                panic!("a candidate")
            };
            let NodeView::Finding(report) = store.node(finding.into()).unwrap() else {
                panic!("a finding")
            };
            (candidate.verdict.unwrap().current, report.current)
        };
        assert_eq!(current(), (true, true));
        mark(PathStatus::Unswept);
        assert_eq!(current(), (false, false));
    }
}
