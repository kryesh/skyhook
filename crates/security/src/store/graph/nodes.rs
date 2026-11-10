//! Nodes created and edited in place, and their statuses. An edit changing anything but the
//! title is a material change of the node; a title-only or restating edit only concerns it.

use std::{collections::BTreeSet, num::NonZeroU32};

use libsql::Row;
use schemars::JsonSchema;
use serde::Deserialize;
use skyhook::db::{Db, Sql as _, enum_column, params};

use crate::store::*;

/// A component and the files and interfaces it owns; one named again moves to it
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewComponent {
    pub(crate) title: Text,
    pub(crate) body: Text,
    #[serde(default)]
    pub(crate) files: Listed<Text>,
    #[serde(default)]
    pub(crate) interfaces: Listed<InterfaceName>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComponentEdit {
    pub(crate) id: ComponentId,
    pub(crate) title: Option<Text>,
    pub(crate) body: Option<Text>,
    #[serde(default)]
    pub(crate) files: Listed<Text>,
    #[serde(default)]
    pub(crate) interfaces: Listed<InterfaceName>,
}

// A node that is its text alone: a state the target keeps.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewText {
    pub(crate) title: Text,
    pub(crate) body: Text,
}

/// Where the path's flow enters a component, and how
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PathStep {
    pub(crate) component: ComponentId,
    pub(crate) location: Option<Location>,
    pub(crate) note: Text,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewPath {
    pub(crate) title: Text,
    pub(crate) body: Text,
    /// Its steps through components, in order, its entry first
    #[serde(default)]
    pub(crate) steps: Listed<PathStep>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PathEdit {
    pub(crate) id: PathId,
    pub(crate) title: Option<Text>,
    pub(crate) body: Option<Text>,
    /// Its whole list of steps, in order
    pub(crate) steps: Option<Listed<PathStep>>,
}

/// An input state with its trust and the read defining it.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewInput {
    pub(crate) title: Text,
    pub(crate) body: Text,
    /// The location of its defining read
    pub(crate) location: Location,
    pub(crate) trust: Trust,
}

/// A state by its scope: an input state, or one the target keeps.
pub(crate) type NewState = StateScoped<NewInput, NewText>;

/// An input state's edit; a state keeps its scope.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateEdit {
    pub(crate) id: StateId,
    pub(crate) title: Option<Text>,
    pub(crate) body: Option<Text>,
    pub(crate) location: Option<Location>,
    pub(crate) trust: Option<Trust>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewPremise {
    pub(crate) title: Text,
    pub(crate) body: Text,
    /// `code`, or `deployment` (the user answers it)
    pub(crate) scope: PremiseScope,
    pub(crate) location: Option<Location>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PremiseEdit {
    pub(crate) id: PremiseId,
    pub(crate) title: Option<Text>,
    pub(crate) body: Option<Text>,
    pub(crate) scope: Option<PremiseScope>,
    pub(crate) location: Option<Location>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewFlaw {
    pub(crate) title: Text,
    pub(crate) body: Text,
    pub(crate) component: ComponentId,
    pub(crate) location: Location,
    pub(crate) cwe: Option<NonZeroU32>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FlawEdit {
    pub(crate) id: FlawId,
    pub(crate) title: Option<Text>,
    pub(crate) body: Option<Text>,
    pub(crate) component: Option<ComponentId>,
    pub(crate) location: Option<Location>,
    pub(crate) cwe: Option<NonZeroU32>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewCandidate {
    pub(crate) title: Text,
    pub(crate) body: Text,
    /// The paths its scenario performs, in order, the first where the attacker starts
    #[serde(rename = "paths")]
    pub(crate) actions: Actions,
    /// Its flaws, its root first
    #[serde(rename = "flaws")]
    pub(crate) chain: Chain,
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CandidateEdit {
    pub(crate) id: CandidateId,
    pub(crate) title: Option<Text>,
    pub(crate) body: Option<Text>,
    /// Its whole list of paths, in order
    #[serde(rename = "paths")]
    pub(crate) actions: Option<Actions>,
    /// Its whole chain, in order
    #[serde(rename = "flaws")]
    pub(crate) chain: Option<Chain>,
}

/// A candidate's flaws, its root first.
pub(crate) type Chain = Listed<FlawId, 1>;
/// The paths a candidate's scenario performs, the first where the attacker starts.
pub(crate) type Actions = Listed<PathId, 1>;

/// A new node: title, body and the fields of its kind
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum NewNode {
    Component(NewComponent),
    Path(NewPath),
    /// By its `scope`, fixed at creation; an input state recorded at a location already
    /// recorded is the one there
    State(NewState),
    Premise(NewPremise),
    Flaw(NewFlaw),
    Candidate(NewCandidate),
}

impl NewNode {
    /// Its title and body.
    fn text(&self) -> (&Text, &Text) {
        match self {
            Self::Component(NewComponent { title, body, .. })
            | Self::Path(NewPath { title, body, .. })
            | Self::State(
                StateScoped::Input(NewInput { title, body, .. })
                | StateScoped::Handed(NewText { title, body })
                | StateScoped::Instance(NewText { title, body })
                | StateScoped::Stored(NewText { title, body }),
            )
            | Self::Premise(NewPremise { title, body, .. })
            | Self::Flaw(NewFlaw { title, body, .. })
            | Self::Candidate(NewCandidate { title, body, .. }) => (title, body),
        }
    }
}

/// An edit of node `id` of its kind: the fields given; the rest stay as they are
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum NodeEdit {
    Component(ComponentEdit),
    Path(PathEdit),
    /// A state keeps its scope; only an input state has a `location` and `trust`
    State(StateEdit),
    Premise(PremiseEdit),
    Flaw(FlawEdit),
    Candidate(CandidateEdit),
}

impl NodeEdit {
    /// The title and body it gives.
    fn text(&self) -> (Option<&Text>, Option<&Text>) {
        let (title, body) = match self {
            Self::Component(ComponentEdit { title, body, .. })
            | Self::Path(PathEdit { title, body, .. })
            | Self::State(StateEdit { title, body, .. })
            | Self::Premise(PremiseEdit { title, body, .. })
            | Self::Flaw(FlawEdit { title, body, .. })
            | Self::Candidate(CandidateEdit { title, body, .. }) => (title, body),
        };
        (title.as_ref(), body.as_ref())
    }
}

/// A node created or edited, its kind's fields typed: a record naming an `id` edits that node.
#[derive(Clone, Debug)]
pub(crate) enum Upsert {
    Create(NewNode),
    Edit(NodeEdit),
}

impl<'de> Deserialize<'de> for Upsert {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let record = serde_json::Map::deserialize(deserializer)?;
        let edits = record.contains_key("id");
        let record = serde_json::Value::Object(record);
        match edits {
            true => NodeEdit::deserialize(record).map(Self::Edit),
            false => NewNode::deserialize(record).map(Self::Create),
        }
        .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Upsert {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "UpsertRecord".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"anyOf": [
            generator.subschema_for::<NewNode>(),
            generator.subschema_for::<NodeEdit>(),
        ]})
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
        names: &Listed<impl std::borrow::Borrow<str>>,
        marks: &mut Marks,
    ) -> Result<bool> {
        let mut changed = false;
        for name in names.iter() {
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

impl Detail for NewPath {
    const KIND: NodeKind = NodeKind::Path;
    fn create(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<()> {
        db.execute("INSERT INTO path(node) VALUES (?1)", params![node])?;
        set_steps(db, node, &self.steps, marks)?;
        Ok(())
    }
}
impl DetailEdit for PathEdit {
    fn edit(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<bool> {
        match &self.steps {
            Some(steps) => set_steps(db, node, steps, marks),
            None => Ok(false),
        }
    }
}

/// A path's step as it is stored: its live component, location and note.
#[derive(PartialEq)]
struct Stepped {
    component: ComponentId,
    location: Option<LocationId>,
    note: Text,
}

impl Entry for Stepped {
    fn read(row: &Row) -> Result<Self> {
        Ok(Self {
            component: ComponentId(row.get(0)?),
            location: row.get::<Option<i64>>(1)?.map(LocationId),
            note: row.get::<String>(2)?.parse()?,
        })
    }
    fn values(&self) -> Vec<libsql::Value> {
        params![self.component, self.location, &self.note]
    }
    fn node(&self) -> NodeId {
        self.component.into()
    }
}

/// Give path `node` `steps` through their live components in order, in place of the ones it
/// has; each component joining or leaving it is concerned: whether they changed.
fn set_steps(db: &Db, node: NodeId, steps: &[PathStep], marks: &mut Marks) -> Result<bool> {
    let wanted = (steps.iter())
        .map(|step| {
            Ok(Stepped {
                component: live(db, step.component)?,
                location: (step.location.as_ref())
                    .map(|at| location_id(db, at))
                    .transpose()?,
                note: step.note.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(STEPS.set(db, node, &wanted, marks)?.is_some())
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
            Self::Input(NewInput { location, .. }) => {
                Ok(input_at(db, location_id(db, location)?)?.map(NodeId::from))
            }
            Self::Handed(_) | Self::Instance(_) | Self::Stored(_) => Ok(None),
        }
    }
    fn create(&self, db: &Db, node: NodeId, _: &mut Marks) -> Result<()> {
        db.execute(
            "INSERT INTO state(node,scope) VALUES (?1,?2)",
            params![node, self.kind()],
        )?;
        if let Self::Input(NewInput {
            location, trust, ..
        }) = self
        {
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

/// The statements reading and replacing one of a node `?1`'s ordered lists: clearing it from
/// position `?2` on.
pub(in crate::store) struct OrderedList {
    held: &'static str,
    clear: &'static str,
    insert: &'static str,
}

macro_rules! listed {
    ($table:literal, $owner:literal, $columns:literal, $values:literal) => {
        OrderedList {
            held: concat!(
                "SELECT ",
                $columns,
                " FROM ",
                $table,
                " WHERE ",
                $owner,
                "=?1 ORDER BY position"
            ),
            clear: concat!(
                "DELETE FROM ",
                $table,
                " WHERE ",
                $owner,
                "=?1 AND position>=?2"
            ),
            insert: concat!(
                "INSERT INTO ",
                $table,
                "(",
                $owner,
                ",position,",
                $columns,
                ") VALUES (?1,?2,",
                $values,
                ")"
            ),
        }
    };
}

pub(in crate::store) const FLAWS: OrderedList =
    listed!("candidate_flaw", "candidate", "flaw", "?3");
pub(in crate::store) const ACTIONS: OrderedList =
    listed!("candidate_action", "candidate", "path", "?3");
const STEPS: OrderedList = listed!("path_step", "path", "component,location,note", "?3,?4,?5");

/// An entry of an ordered list: read from its row and written as its values, naming one node.
pub(in crate::store) trait Entry: PartialEq + Sized {
    fn read(row: &Row) -> Result<Self>;
    fn values(&self) -> Vec<libsql::Value>;
    fn node(&self) -> NodeId;
}

impl<I: KindId + PartialEq> Entry for I {
    fn read(row: &Row) -> Result<Self> {
        Ok(I::of(NodeId(row.get(0)?)))
    }
    fn values(&self) -> Vec<libsql::Value> {
        params![self.node()]
    }
    fn node(&self) -> NodeId {
        (*self).into()
    }
}

impl OrderedList {
    /// Node `node`'s list, in order.
    pub(in crate::store) fn held<E: Entry>(
        &self,
        db: &Db,
        node: impl Into<NodeId>,
    ) -> Result<Vec<E>> {
        rows(db, self.held, params![node.into()], E::read)
    }

    /// Give node `node` the list `wanted` in place of the one it has, rewriting it past the
    /// prefix they share, each node joining or leaving it concerned: the list it held, unless
    /// that was `wanted` already.
    pub(in crate::store) fn set<E: Entry>(
        &self,
        db: &Db,
        node: impl Into<NodeId>,
        wanted: &[E],
        marks: &mut Marks,
    ) -> Result<Option<Vec<E>>> {
        let node = node.into();
        let held = self.held(db, node)?;
        if held == wanted {
            return Ok(None);
        }
        let kept = (held.iter().zip(wanted))
            .take_while(|(held, wanted)| held == wanted)
            .count();
        db.execute(self.clear, params![node, kept])?;
        for (position, entry) in wanted.iter().enumerate().skip(kept) {
            let mut values = params![node, position];
            values.extend(entry.values());
            db.execute(self.insert, values)?;
        }
        let members = |list: &[E]| -> BTreeSet<NodeId> { list.iter().map(E::node).collect() };
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
    let (mut wanted, mut seen) = (Vec::with_capacity(chain.len()), BTreeSet::new());
    for flaw in chain.iter() {
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
    let wanted = (actions.iter())
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

fn create<D: Detail>(db: &Db, actor: &AgentId, new: &D, text: (&Text, &Text)) -> Result<NodeId> {
    if let Some(node) = new.recorded(db)? {
        return Ok(node);
    }
    audit(db, actor, EventKind::Created, None, |_| {
        let node = NodeId(db.insert(
            "INSERT INTO node(kind,title,body) VALUES (?1,?2,?3)",
            params![D::KIND, text.0, text.1],
        )?);
        let mut marks = Marks::of(node, EventEffect::Changed);
        new.create(db, node, &mut marks)?;
        Ok((node, marks))
    })
}

fn edit(
    db: &Db,
    actor: &AgentId,
    id: impl KindId,
    edit: &impl DetailEdit,
    (title, body): (Option<&Text>, Option<&Text>),
) -> Result<NodeId> {
    let node: NodeId = live(db, id)?.into();
    audit(db, actor, EventKind::Edited, None, |_| {
        if let Some(title) = title {
            set(
                db,
                "UPDATE node SET title=?2 WHERE id=?1 AND title!=?2",
                node,
                title.sql(),
            )?;
        }
        let mut marks = Marks::default();
        let mut changed = edit.edit(db, node, &mut marks)?;
        if let Some(body) = body {
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

/// A code premise is decided within its settle, or within the review of a candidate whose
/// chain rests on it; a deployment premise, the user's to answer, outside every task.
const PREMISE: Standing = standing_of!(
    "premise_standing",
    "node",
    "status",
    "proposed",
    concat!(
        "CASE (SELECT scope FROM premise WHERE node=?1) WHEN 'code' THEN ",
        crate::store::ledger::standing::owns!("settle", "?2", "?1"),
        " OR EXISTS (SELECT 1 FROM (",
        crate::store::ledger::facts::premise_candidates!("?1"),
        ") rc WHERE ",
        crate::store::ledger::standing::owns!("review", "?2", "rc.candidate"),
        ") ELSE NOT EXISTS (SELECT 1 FROM event_task WHERE event=?2) END"
    ),
    Changed
);
const FLAW: Standing = standing_of!(
    "flaw_standing",
    "node",
    "status",
    "proposed",
    crate::store::ledger::standing::owns!("chain", "?2", "?1"),
    Changed
);

/// A node of its kind and the status it takes
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StatusChange {
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
    State(StateMark),
    Component {
        id: ComponentId,
        status: InventoryStatus,
    },
}

/// A state marked by its `status`: its check across calls done, resting on the premises its
/// clearing relies on, or its inventory status.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StateMark {
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

// The premises a check's clearing rests on: at least one.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
// Read through `Option` so that a missing list is refused with the empty one's guidance.
#[serde(try_from = "Option<Vec<PremiseId>>")]
#[schemars(extend("minItems" = 1, "maxItems" = MAX_ATTACHMENTS))]
pub(crate) struct Premises(Listed<PremiseId, 1>);

impl TryFrom<Option<Vec<PremiseId>>> for Premises {
    type Error = Error;

    fn try_from(premises: Option<Vec<PremiseId>>) -> Result<Self> {
        match Listed::try_from(premises.unwrap_or_default()) {
            Err(Error::Empty(_)) => Err(Error::Unrested),
            listed => listed.map(Self),
        }
    }
}

impl IntoIterator for Premises {
    type Item = PremiseId;
    type IntoIter = std::vec::IntoIter<PremiseId>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl Store {
    /// Create a node or edit one, returning its id: a merged node's edit goes to the node it
    /// was merged into, and an input state at a location already recorded is the one there.
    pub(crate) fn upsert(&self, actor: &AgentId, upsert: &Upsert) -> Result<NodeId> {
        self.write(|db| match upsert {
            Upsert::Create(new) => {
                let text = new.text();
                match new {
                    NewNode::Component(new) => create(db, actor, new, text),
                    NewNode::Path(new) => create(db, actor, new, text),
                    NewNode::State(new) => create(db, actor, new, text),
                    NewNode::Premise(new) => create(db, actor, new, text),
                    NewNode::Flaw(new) => create(db, actor, new, text),
                    NewNode::Candidate(new) => create(db, actor, new, text),
                }
            }
            Upsert::Edit(change) => {
                let text = change.text();
                match change {
                    NodeEdit::Component(change) => edit(db, actor, change.id, change, text),
                    NodeEdit::Path(change) => edit(db, actor, change.id, change, text),
                    NodeEdit::State(change) => edit(db, actor, change.id, change, text),
                    NodeEdit::Premise(change) => edit(db, actor, change.id, change, text),
                    NodeEdit::Flaw(change) => edit(db, actor, change.id, change, text),
                    NodeEdit::Candidate(change) => edit(db, actor, change.id, change, text),
                }
            }
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
    pub(crate) fn set_status(
        &self,
        actor: &AgentId,
        change: StatusChange,
        note: &Text,
    ) -> Result<()> {
        self.write(|db| {
            audit(db, actor, EventKind::Status, Some(note), |event| {
                let mut marks = Marks::default();
                match change {
                    StatusChange::Premise { id, status } => {
                        let node = live(db, id)?;
                        let sql = "INSERT INTO premise_decision(event,premise,status,owned) \
                                   VALUES (?1,?2,?3,?4)";
                        let row = params![event, node, status];
                        marks.mark(node, PREMISE.record(db, node, event, status, sql, row)?);
                    }
                    StatusChange::Flaw { id, status } => {
                        let node = live(db, id)?;
                        let sql = "INSERT INTO flaw_judgement(event,flaw,status,owned) VALUES \
                                   (?1,?2,?3,?4)";
                        let row = params![event, node, status];
                        marks.mark(node, FLAW.record(db, node, event, status, sql, row)?);
                    }
                    StatusChange::Path { id, status } => {
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
                    StatusChange::State(StateMark::Checked { id, rests_on }) => {
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
                    StatusChange::State(StateMark::Recorded { id }) => {
                        inventory(db, id, InventoryStatus::Recorded, &mut marks)?;
                    }
                    StatusChange::State(StateMark::Retracted { id }) => {
                        inventory(db, id, InventoryStatus::Retracted, &mut marks)?;
                    }
                    StatusChange::Component { id, status } => {
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

/// Set state `id`'s inventory status, moving it and the paths accessing it alone.
fn inventory(db: &Db, id: StateId, status: InventoryStatus, marks: &mut Marks) -> Result<()> {
    let node = live(db, id)?;
    let sql = "UPDATE state SET status=?2 WHERE node=?1";
    db.execute(sql, params![node, status])?;
    let sql = "SELECT DISTINCT path FROM path_state WHERE state=?1";
    for path in rows(db, sql, params![node], |row| Ok(NodeId(row.get(0)?)))? {
        marks.alone(path);
    }
    marks.alone(node);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use serde_json::json;
    use skyhook::job::JobState;

    use super::*;
    use crate::store::tests::{child_agent, fixture, last_event, marks, scenario, text};

    /// A record as `upsert` reads one: an edit with an `id`, else a new node.
    fn read(value: serde_json::Value) -> serde_json::Result<Upsert> {
        serde_json::from_value(value)
    }

    fn parse(value: serde_json::Value) -> Upsert {
        read(value).unwrap()
    }

    /// Each kind is created with its fields and edited in place, a candidate's chain included:
    /// an edit of anything but the title changes the node, while a title or a restatement only
    /// concerns it. A create lacking its kind's fields is refused.
    #[test]
    fn nodes_are_created_and_edited_in_place() {
        let (_session, store, actor) = fixture();
        let upsert = |value| store.upsert(&actor, &parse(value)).unwrap();
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
        let moved = parse(json!({"kind": "state", "id": elsewhere, "location": at}));
        assert_matches!(
            store.upsert(&actor, &moved),
            Err(Error::StateAt(at)) if NodeId::from(at) == input
        );
        let window = upsert(
            json!({"kind": "state", "title": "window", "body": "ring buffer",
            "scope": "instance"}),
        );
        let trusted = parse(json!({"kind": "state", "id": window, "trust": "local"}));
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
        for (paths, flaws, empty) in [
            (json!([path]), json!([]), NodeKind::Flaw),
            (json!([]), json!([flaw]), NodeKind::Path),
        ] {
            let refused = read(json!({"kind": "candidate", "title": "t", "body": "b",
                "paths": paths, "flaws": flaws}));
            let error = refused.unwrap_err().to_string();
            assert!(error.contains(&Error::Empty(empty).to_string()), "{error}");
        }

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
            let change = StatusChange::Path { id: path, status };
            store
                .set_status(&stranger, change, &text("marked"))
                .unwrap();
            let NodeView::Path(view) = store.node(path.into()).unwrap() else {
                panic!("a path")
            };
            let listed = store.list(&ListQuery::Path {}, Page::default()).unwrap();
            let ListItem::Node(listed) = &listed[0] else {
                panic!("a node")
            };
            assert_eq!(
                (view.status, listed.status),
                (status, Some(NodeStatus::Path(status)))
            );
            assert_eq!(marks(&store, last_event(&store)), moved);
        }
    }
}
