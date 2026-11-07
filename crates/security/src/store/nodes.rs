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

/// A state by its scope: an input state with its trust and the read defining it, or one the
/// target keeps.
#[derive(Clone, Debug)]
pub enum NewState {
    Input { location: Location, trust: Trust },
    Handed,
    Instance,
    Stored,
}

impl NewState {
    fn scope(&self) -> StateScope {
        match self {
            Self::Input { .. } => StateScope::Input,
            Self::Handed => StateScope::Handed,
            Self::Instance => StateScope::Instance,
            Self::Stored => StateScope::Stored,
        }
    }
}

/// An input state's edit; a state keeps its scope.
#[derive(Clone, Debug, Default)]
pub struct StateEdit {
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
            1..=MAX_ATTACHMENTS => Ok(Self(ids)),
            _ => Err(Error::Limit(Bound::Attachments)),
        }
    }
}

/// A node created or edited, its kind's fields typed.
#[derive(Clone, Debug)]
pub enum Upsert {
    Component(Change<ComponentId, NewComponent, ComponentEdit>),
    Path(Change<PathId, (), ()>),
    State(Change<StateId, NewState, StateEdit>),
    Premise(Change<PremiseId, NewPremise, PremiseEdit>),
    Flaw(Change<FlawId, NewFlaw, FlawEdit>),
    Candidate(Change<CandidateId, NewCandidate, CandidateEdit>),
}

/// A new node: title, body and the fields of its kind
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NewRecord {
    Component {
        title: Text,
        body: Text,
        #[serde(default)]
        #[schemars(length(max = MAX_ATTACHMENTS))]
        files: Vec<Text>,
        #[serde(default)]
        #[schemars(length(max = MAX_ATTACHMENTS))]
        interfaces: Vec<InterfaceName>,
    },
    Path {
        title: Text,
        body: Text,
    },
    /// By its `scope`, fixed at creation
    State(NewStateRecord),
    Premise {
        title: Text,
        body: Text,
        /// `code`, or `deployment` (the user answers it)
        scope: PremiseScope,
        location: Option<Location>,
    },
    Flaw {
        title: Text,
        body: Text,
        component: ComponentId,
        location: Location,
        cwe: Option<NonZeroU32>,
    },
    Candidate {
        title: Text,
        body: Text,
        /// The paths its scenario performs, in order, the first where the attacker starts
        #[schemars(length(min = 1, max = MAX_ATTACHMENTS))]
        paths: Vec<PathId>,
        /// Its flaws, its root first
        #[schemars(length(min = 1, max = MAX_ATTACHMENTS))]
        flaws: Vec<FlawId>,
    },
}

/// A new state by its scope.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum NewStateRecord {
    /// Actor-written data the target takes in, with the `location` of its defining read; one
    /// recorded at a location already recorded returns the one there
    Input {
        title: Text,
        body: Text,
        location: Location,
        trust: Trust,
    },
    /// Target-written, carried by the caller into later calls
    Handed { title: Text, body: Text },
    /// One object's fields across its calls
    Instance { title: Text, body: Text },
    /// Outlives the caller's interaction, read by other flows
    Stored { title: Text, body: Text },
}

/// An edit of node `id` of its kind: the fields given; the rest stay as they are
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditRecord {
    Component {
        id: ComponentId,
        title: Option<Text>,
        body: Option<Text>,
        #[serde(default)]
        #[schemars(length(max = MAX_ATTACHMENTS))]
        files: Vec<Text>,
        #[serde(default)]
        #[schemars(length(max = MAX_ATTACHMENTS))]
        interfaces: Vec<InterfaceName>,
    },
    Path {
        id: PathId,
        title: Option<Text>,
        body: Option<Text>,
    },
    /// A state keeps its scope; only an input state has a `location` and `trust`
    State {
        id: StateId,
        title: Option<Text>,
        body: Option<Text>,
        location: Option<Location>,
        trust: Option<Trust>,
    },
    Premise {
        id: PremiseId,
        title: Option<Text>,
        body: Option<Text>,
        scope: Option<PremiseScope>,
        location: Option<Location>,
    },
    Flaw {
        id: FlawId,
        title: Option<Text>,
        body: Option<Text>,
        component: Option<ComponentId>,
        location: Option<Location>,
        cwe: Option<NonZeroU32>,
    },
    Candidate {
        id: CandidateId,
        title: Option<Text>,
        body: Option<Text>,
        /// Its whole list of paths, in order
        #[schemars(length(min = 1, max = MAX_ATTACHMENTS))]
        paths: Option<Vec<PathId>>,
        /// Its whole chain, in order
        #[schemars(length(min = 1, max = MAX_ATTACHMENTS))]
        flaws: Option<Vec<FlawId>>,
    },
}

impl<I, N, E> Change<I, N, E> {
    fn created(title: Text, body: Text, detail: N) -> Self {
        Self::Create(New {
            title,
            body,
            detail,
        })
    }

    fn edited(id: I, title: Option<Text>, body: Option<Text>, detail: E) -> Self {
        Self::Edit(
            id,
            Edit {
                title,
                body,
                detail,
            },
        )
    }
}

impl TryFrom<NewRecord> for Upsert {
    type Error = Error;
    fn try_from(record: NewRecord) -> Result<Self> {
        Ok(match record {
            NewRecord::Component {
                title,
                body,
                files,
                interfaces,
            } => Self::Component(Change::created(
                title,
                body,
                NewComponent { files, interfaces },
            )),
            NewRecord::Path { title, body } => Self::Path(Change::created(title, body, ())),
            NewRecord::State(state) => {
                let (title, body, state) = match state {
                    NewStateRecord::Input {
                        title,
                        body,
                        location,
                        trust,
                    } => (title, body, NewState::Input { location, trust }),
                    NewStateRecord::Handed { title, body } => (title, body, NewState::Handed),
                    NewStateRecord::Instance { title, body } => (title, body, NewState::Instance),
                    NewStateRecord::Stored { title, body } => (title, body, NewState::Stored),
                };
                Self::State(Change::created(title, body, state))
            }
            NewRecord::Premise {
                title,
                body,
                scope,
                location,
            } => Self::Premise(Change::created(title, body, NewPremise { scope, location })),
            NewRecord::Flaw {
                title,
                body,
                component,
                location,
                cwe,
            } => Self::Flaw(Change::created(
                title,
                body,
                NewFlaw {
                    component,
                    location,
                    cwe,
                },
            )),
            NewRecord::Candidate {
                title,
                body,
                paths,
                flaws,
            } => Self::Candidate(Change::created(
                title,
                body,
                NewCandidate {
                    actions: paths.try_into()?,
                    chain: flaws.try_into()?,
                },
            )),
        })
    }
}

impl TryFrom<EditRecord> for Upsert {
    type Error = Error;
    fn try_from(record: EditRecord) -> Result<Self> {
        Ok(match record {
            EditRecord::Component {
                id,
                title,
                body,
                files,
                interfaces,
            } => Self::Component(Change::edited(
                id,
                title,
                body,
                ComponentEdit { files, interfaces },
            )),
            EditRecord::Path { id, title, body } => Self::Path(Change::edited(id, title, body, ())),
            EditRecord::State {
                id,
                title,
                body,
                location,
                trust,
            } => Self::State(Change::edited(
                id,
                title,
                body,
                StateEdit { location, trust },
            )),
            EditRecord::Premise {
                id,
                title,
                body,
                scope,
                location,
            } => Self::Premise(Change::edited(
                id,
                title,
                body,
                PremiseEdit { scope, location },
            )),
            EditRecord::Flaw {
                id,
                title,
                body,
                component,
                location,
                cwe,
            } => Self::Flaw(Change::edited(
                id,
                title,
                body,
                FlawEdit {
                    component,
                    location,
                    cwe,
                },
            )),
            EditRecord::Candidate {
                id,
                title,
                body,
                paths,
                flaws,
            } => Self::Candidate(Change::edited(
                id,
                title,
                body,
                CandidateEdit {
                    actions: paths.map(Actions::try_from).transpose()?,
                    chain: flaws.map(Chain::try_from).transpose()?,
                },
            )),
        })
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

/// The input state read at `location`: only a live input state holds one.
fn input_at(db: &Db, location: LocationId) -> Result<Option<StateId>> {
    optional(
        db,
        "SELECT state FROM input_state WHERE location=?1",
        params![location],
        |row| Ok(StateId(row.get(0)?)),
    )
}

impl Detail for NewState {
    const KIND: NodeKind = NodeKind::State;
    fn recorded(&self, db: &Db) -> Result<Option<NodeId>> {
        match self {
            Self::Input { location, .. } => {
                Ok(input_at(db, location_id(db, location)?)?.map(NodeId::from))
            }
            Self::Handed | Self::Instance | Self::Stored => Ok(None),
        }
    }
    fn create(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<()> {
        db.execute(
            "INSERT INTO state(node,scope) VALUES (?1,?2)",
            params![node, self.scope()],
        )?;
        if let Self::Input { location, trust } = self {
            db.execute(
                "INSERT INTO input_state(state,location,trust) VALUES (?1,?2,?3)",
                params![node, location_id(db, location)?, *trust],
            )?;
        }
        Ok(())
    }
}
impl DetailEdit for StateEdit {
    fn edit(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<bool> {
        if self.location.is_none() && self.trust.is_none() {
            return Ok(false);
        }
        if !exists(
            db,
            "SELECT 1 FROM input_state WHERE state=?1",
            params![node],
        )? {
            return Err(Error::NotInput);
        }
        let mut changed = false;
        if let Some(location) = &self.location {
            let location = location_id(db, location)?;
            if let Some(state) = input_at(db, location)?
                && NodeId::from(state) != node
            {
                return Err(Error::StateAt(state));
            }
            changed |= set(
                db,
                "UPDATE input_state SET location=?2 WHERE state=?1 AND location!=?2",
                node,
                location.sql(),
            )?;
        }
        if let Some(trust) = self.trust {
            changed |= set(
                db,
                "UPDATE input_state SET trust=?2 WHERE state=?1 AND trust!=?2",
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

/// A code premise is decided within its settle, or within the review of a candidate whose
/// chain rests on it; a deployment premise, the user's to answer, outside every task.
const PREMISE: Standing = standing_of!(
    "premise_standing",
    "node",
    "status",
    "proposed",
    concat!(
        "CASE (SELECT scope FROM premise WHERE node=?1) WHEN 'code' THEN ",
        crate::store::bearing::owns!("settle", "?2", "?1"),
        " OR EXISTS (SELECT 1 FROM (",
        crate::store::bearing::premise_candidates!("?1"),
        ") rc WHERE ",
        crate::store::bearing::owns!("review", "?2", "rc.candidate"),
        ") ELSE NOT EXISTS (SELECT 1 FROM event_task WHERE event=?2) END"
    ),
    Changed
);
const FLAW: Standing = standing_of!(
    "flaw_standing",
    "node",
    "status",
    "proposed",
    crate::store::bearing::owns!("chain", "?2", "?1"),
    Changed
);

/// A status set: a premise decided, a flaw judged, a path marked swept or unswept, a state
/// checked, or a state, path or component retracted or restored.
#[derive(Clone, Debug)]
pub enum StatusChange {
    Premise(PremiseId, PremiseStatus),
    Flaw(FlawId, FlawStatus),
    Path(PathId, PathStatus),
    State(StateId, StateMark),
    Component(ComponentId, InventoryStatus),
}

/// What marking a state records: its check across calls done, resting on the premises its
/// clearing relies on, or its inventory status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateMark {
    Checked { rests_on: Premises },
    Inventory(InventoryStatus),
}

// The premises a check's clearing rests on: at least one.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
// Read through `Option` so that a missing list is refused with the empty one's guidance.
#[serde(try_from = "Option<Vec<PremiseId>>")]
#[schemars(extend("minItems" = 1))]
pub struct Premises(Vec<PremiseId>);

impl TryFrom<Option<Vec<PremiseId>>> for Premises {
    type Error = Error;

    fn try_from(premises: Option<Vec<PremiseId>>) -> Result<Self> {
        (premises.filter(|premises| !premises.is_empty()))
            .map(Self)
            .ok_or(Error::Unrested)
    }
}

impl IntoIterator for Premises {
    type Item = PremiseId;
    type IntoIter = std::vec::IntoIter<PremiseId>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// A node of its kind and the status it takes
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StatusRecord {
    Premise {
        id: PremiseId,
        status: PremiseStatus,
    },
    Flaw {
        id: FlawId,
        status: FlawStatus,
    },
    Path {
        id: PathId,
        status: PathStatus,
    },
    /// By its `status`
    State(StateRecord),
    Component {
        id: ComponentId,
        status: InventoryStatus,
    },
}

/// A state marked by its `status`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateRecord {
    Checked {
        id: StateId,
        /// Premises its clearing rests on: a recorded one, or a general claim the recorded
        /// accesses do not show
        rests_on: Premises,
    },
    Recorded {
        id: StateId,
    },
    Retracted {
        id: StateId,
    },
}

impl From<StatusRecord> for StatusChange {
    fn from(record: StatusRecord) -> Self {
        match record {
            StatusRecord::Premise { id, status } => Self::Premise(id, status),
            StatusRecord::Flaw { id, status } => Self::Flaw(id, status),
            StatusRecord::Path { id, status } => Self::Path(id, status),
            StatusRecord::State(StateRecord::Checked { id, rests_on }) => {
                Self::State(id, StateMark::Checked { rests_on })
            }
            StatusRecord::State(StateRecord::Recorded { id }) => {
                Self::State(id, StateMark::Inventory(InventoryStatus::Recorded))
            }
            StatusRecord::State(StateRecord::Retracted { id }) => {
                Self::State(id, StateMark::Inventory(InventoryStatus::Retracted))
            }
            StatusRecord::Component { id, status } => Self::Component(id, status),
        }
    }
}

impl Store {
    /// The kind of node `id`.
    pub fn kind(&self, id: NodeId) -> Result<NodeKind> {
        self.read(|db| live_node(db, id).map(|(_, kind)| kind))
    }

    /// Create a node or edit one, returning its id: a merged node's edit goes to the node it
    /// was merged into, and an input state at a location already recorded is the one there.
    pub fn upsert(&self, actor: &AgentId, upsert: &Upsert) -> Result<NodeId> {
        self.write(|db| match upsert {
            Upsert::Component(change) => apply(db, actor, change),
            Upsert::Path(change) => apply(db, actor, change),
            Upsert::State(change) => apply(db, actor, change),
            Upsert::Premise(change) => apply(db, actor, change),
            Upsert::Flaw(change) => apply(db, actor, change),
            Upsert::Candidate(change) => apply(db, actor, change),
        })
    }

    /// Set a status, with the actor's reason. A premise's or flaw's status set by the work owning
    /// it changes it when it differs from the standing, and only concerns it otherwise. Set
    /// anywhere else it is a proposal: one differing from the standing disputes it, moving that
    /// work alone, and one agreeing only concerns it. Marking a path, state or component moves
    /// its own standing alone, and a state's that of the paths accessing it, so an unswept or
    /// restored path is offered its sweep again. A path retracted or restored also moves the standing of the candidates
    /// performing it, whose chains gain or lose the premises it relies on, and the checks of the
    /// states it accesses, which gain or lose its accesses. A state marked checked records this
    /// event as its check, kept only by an instance or handed state, beside the premises it rests
    /// on, which it only concerns.
    pub fn set_status(&self, actor: &AgentId, change: StatusChange, note: &Text) -> Result<()> {
        self.write(|db| {
            audit(db, actor, EventKind::Status, Some(note), |event| {
                let mut marks = Marks::default();
                match change {
                    StatusChange::Premise(id, status) => {
                        let node = live(db, id)?;
                        let sql = "INSERT INTO premise_decision(event,premise,status,owned) \
                                   VALUES (?1,?2,?3,?4)";
                        let row = params![event, node, status];
                        marks.mark(node, PREMISE.record(db, node, event, status, sql, row)?);
                    }
                    StatusChange::Flaw(id, status) => {
                        let node = live(db, id)?;
                        let sql = "INSERT INTO flaw_judgement(event,flaw,status,owned) VALUES \
                                   (?1,?2,?3,?4)";
                        let row = params![event, node, status];
                        marks.mark(node, FLAW.record(db, node, event, status, sql, row)?);
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
                            true => {
                                marks.mark(node, EventEffect::Standing);
                                marks.accesses(db, node)?;
                            }
                            false => marks.alone(node),
                        }
                    }
                    StatusChange::State(id, StateMark::Checked { rests_on }) => {
                        let node = live(db, id)?;
                        let sql = "INSERT INTO state_check(state,scope,event) SELECT node,scope,?2 FROM state \
                                   WHERE node=?1 AND scope IN ('instance','handed') ON \
                                   CONFLICT(state) DO UPDATE SET event=excluded.event";
                        db.execute(sql, params![node, event])?;
                        for premise in rests_on {
                            let premise = live(db, premise)?;
                            let sql = "INSERT INTO state_premise(state,premise) SELECT state,?2 \
                                       FROM state_check WHERE state=?1 ON CONFLICT DO NOTHING";
                            db.execute(sql, params![node, premise])?;
                            marks.mark(premise, EventEffect::Concerns);
                        }
                        marks.alone(node);
                    }
                    StatusChange::State(id, StateMark::Inventory(status)) => {
                        let node = live(db, id)?;
                        let sql = "UPDATE state SET status=?2 WHERE node=?1";
                        db.execute(sql, params![node, status])?;
                        let sql = "SELECT DISTINCT path FROM path_state WHERE state=?1";
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
    use std::assert_matches;

    use serde_json::json;
    use skyhook::job::JobState;

    use super::{
        super::tests::{child_agent, fixture, last_event, marks, scenario, text},
        *,
    };

    /// A record as `upsert` reads one: an edit with an `id`, else a new node.
    fn read(value: serde_json::Value) -> serde_json::Result<Result<Upsert>> {
        Ok(match value.get("id") {
            Some(_) => serde_json::from_value::<EditRecord>(value)?.try_into(),
            None => serde_json::from_value::<NewRecord>(value)?.try_into(),
        })
    }

    fn parse(value: serde_json::Value) -> Result<Upsert> {
        read(value).unwrap()
    }

    /// Each kind is created with its fields and edited in place, a candidate's chain included:
    /// an edit of anything but the title changes the node, while a title or a restatement only
    /// concerns it. A create
    /// lacking its kind's fields is refused, and a detail row holds only a node of its kind.
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
        let input = upsert(json!({"kind": "state", "title": "socket", "body": "reads",
            "scope": "input", "location": at, "trust": "untrusted"}));
        let recorded = last_event(&store);
        assert_eq!(
            upsert(json!({"kind": "state", "title": "again", "body": "reads",
                "scope": "input", "location": at, "trust": "local"})),
            input,
            "an input state read at a recorded location is the one there"
        );
        assert_eq!(last_event(&store), recorded);
        let elsewhere = upsert(json!({"kind": "state", "title": "pipe", "body": "reads",
            "scope": "input", "location": {"code": {"file": "pipe.c", "start_line": 1,
            "end_line": 1}}, "trust": "local"}));
        let moved = parse(json!({"kind": "state", "id": elsewhere, "location": at})).unwrap();
        assert_matches!(
            store.upsert(&actor, &moved),
            Err(Error::StateAt(at)) if NodeId::from(at) == input
        );
        let window = upsert(
            json!({"kind": "state", "title": "window", "body": "ring buffer",
            "scope": "instance"}),
        );
        let trusted = parse(json!({"kind": "state", "id": window, "trust": "local"})).unwrap();
        assert_matches!(store.upsert(&actor, &trusted), Err(Error::NotInput));
        for refused in [
            json!({"kind": "state", "title": "t", "body": "b", "scope": "stored",
                "trust": "local"}),
            json!({"kind": "state", "id": window, "scope": "stored"}),
            json!({"kind": "state", "title": "t", "body": "b", "scope": "input",
                "location": at}),
        ] {
            assert!(read(refused.clone()).is_err(), "{refused}");
        }
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
        let kinds = [decoder, input, path, premise, flaw, candidate].map(|id| {
            match store.node(id).unwrap() {
                NodeView::Component(_) => NodeKind::Component,
                NodeView::State(_) => NodeKind::State,
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
                NodeKind::State,
                NodeKind::Path,
                NodeKind::Premise,
                NodeKind::Flaw,
                NodeKind::Candidate
            ]
        );
        assert!(read(json!({"kind": "flaw", "title": "copy"})).is_err());
        let empty = |paths, flaws| {
            parse(
                json!({"kind": "candidate", "title": "t", "body": "b", "paths": paths,
                "flaws": flaws}),
            )
        };
        assert_matches!(
            empty(json!([path]), json!([])),
            Err(Error::Empty(NodeKind::Flaw))
        );
        assert_matches!(
            empty(json!([]), json!([flaw])),
            Err(Error::Empty(NodeKind::Path))
        );

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
    /// also moves the candidates performing it and the states it accesses.
    #[test]
    fn any_agent_marks_a_paths_status() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let (path, candidate) = (scenario.path, scenario.candidate);
        let NodeView::Path(view) = store.node(path.into()).unwrap() else {
            panic!("a path")
        };
        let state = view.states[0].id;
        let (stranger, _) = child_agent(&store, JobState::Running);
        let alone = vec![(path.into(), EventEffect::Standing)];
        let carried = vec![
            (path.into(), EventEffect::Standing),
            (state.into(), EventEffect::Standing),
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
}
