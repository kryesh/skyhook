//! Nodes created and edited in place, and their statuses. An edit changing anything but the
//! title is a material change of the node; a title-only or restating edit only concerns it.

use std::num::NonZeroU32;

use schemars::JsonSchema;
use serde::Deserialize;
use skyhook::db::{Db, Sql as _, params};

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

/// A component and the files it owns; a file named again moves to it.
#[derive(Clone, Debug)]
pub struct NewComponent {
    pub files: Vec<Text>,
}
#[derive(Clone, Debug, Default)]
pub struct ComponentEdit {
    pub files: Vec<Text>,
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

/// A candidate along `path`, chaining `chain` in order from its root.
#[derive(Clone, Debug)]
pub struct NewCandidate {
    pub path: PathId,
    pub chain: Chain,
}
/// A candidate's edit: its path, or its whole chain in order.
#[derive(Clone, Debug, Default)]
pub struct CandidateEdit {
    pub path: Option<PathId>,
    pub chain: Option<Chain>,
}

/// A candidate's flaws, at least one, its root first.
#[derive(Clone, Debug)]
pub struct Chain(Vec<FlawId>);

impl TryFrom<Vec<FlawId>> for Chain {
    type Error = Error;
    fn try_from(flaws: Vec<FlawId>) -> Result<Self> {
        match flaws.len() {
            0 => Err(Error::EmptyChain),
            MAX_ATTACHMENTS.. => Err(Error::Limit(Bound::Attachments)),
            _ => Ok(Self(flaws)),
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
        path: Option<PathId>,
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
            Component [] [files] => NewComponent { files }, ComponentEdit { files };
            Path [] [] => (), ();
            Source [location, trust] [] =>
                NewSource { location, trust }, SourceEdit { location, trust };
            Premise [scope] [location] =>
                NewPremise { scope, location }, PremiseEdit { scope, location };
            Flaw [component, location] [cwe] =>
                NewFlaw { component, location, cwe }, FlawEdit { component, location, cwe };
            Candidate [path, flaws] [] =>
                NewCandidate { path, chain: flaws.try_into()? },
                CandidateEdit { path, chain: flaws.map(Chain::try_from).transpose()? };
        ))
    }
}

/// The id of `location`, recorded on first use.
pub(super) fn location_id(db: &Db, location: &Location) -> Result<LocationId> {
    let key = || {
        params![
            &location.file,
            location.start_line,
            location.end_line,
            location.function.as_ref()
        ]
    };
    db.execute(
        "INSERT INTO location(file,start_line,end_line,function) VALUES (?1,?2,?3,?4) ON \
         CONFLICT DO NOTHING",
        key(),
    )?;
    stored(
        db,
        "SELECT id FROM location WHERE file=?1 AND start_line=?2 AND end_line=?3 AND function IS \
         ?4",
        key(),
        |row| Ok(LocationId(row.get(0)?)),
    )
}

/// Run `sql`, an update of node `?1` to `?2` that matches only when the value differs: whether
/// it changed.
fn set(db: &Db, sql: &'static str, node: NodeId, value: libsql::Value) -> Result<bool> {
    Ok(db.execute(sql, vec![node.sql(), value])? > 0)
}

/// `files` owned by `component`; each one owned elsewhere before is marked changed with its
/// owner: whether any was new to it.
fn own_files(db: &Db, component: NodeId, files: &[Text], marks: &mut Marks) -> Result<bool> {
    if files.len() > MAX_ATTACHMENTS {
        return Err(Error::Limit(Bound::Attachments));
    }
    let mut changed = false;
    for file in files {
        let owner = optional(
            db,
            "SELECT component FROM component_file WHERE file=?1",
            params![file],
            |row| Ok(NodeId(row.get(0)?)),
        )?;
        if owner == Some(component) {
            continue;
        }
        db.execute(
            "INSERT INTO component_file(file,component) VALUES (?1,?2) ON CONFLICT(file) DO \
             UPDATE SET component=excluded.component",
            params![file, component],
        )?;
        if let Some(owner) = owner {
            marks.mark(owner, EventEffect::Changed);
        }
        changed = true;
    }
    Ok(changed)
}

/// The kind fields of a new node: insert its detail row and anything it holds, marking the
/// other nodes it bears on.
trait Detail {
    const KIND: NodeKind;
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
        own_files(db, node, &self.files, marks)?;
        Ok(())
    }
}
impl DetailEdit for ComponentEdit {
    fn edit(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<bool> {
        own_files(db, node, &self.files, marks)
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

impl Detail for NewSource {
    const KIND: NodeKind = NodeKind::Source;
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
            changed |= set(
                db,
                "UPDATE source SET location=?2 WHERE node=?1 AND location!=?2",
                node,
                location_id(db, location)?.sql(),
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

/// Chain `chain`'s live flaws in order to candidate `node`, in place of the chain it has: each
/// flaw joining or leaving it is concerned, and one taking or losing its root role moved;
/// whether the chain changed.
fn set_chain(db: &Db, node: NodeId, chain: &Chain, marks: &mut Marks) -> Result<bool> {
    let mut wanted = Vec::with_capacity(chain.0.len());
    for flaw in &chain.0 {
        let flaw = live(db, *flaw)?;
        if !wanted.contains(&flaw) {
            wanted.push(flaw);
        }
    }
    let held = rows(
        db,
        "SELECT flaw FROM candidate_flaw WHERE candidate=?1 ORDER BY position",
        params![node],
        |row| Ok(FlawId(row.get(0)?)),
    )?;
    if held == wanted {
        return Ok(false);
    }
    db.execute(
        "DELETE FROM candidate_flaw WHERE candidate=?1",
        params![node],
    )?;
    for (position, flaw) in wanted.iter().enumerate() {
        db.execute(
            "INSERT INTO candidate_flaw(candidate,position,flaw) VALUES (?1,?2,?3)",
            params![node, position, *flaw],
        )?;
    }
    for flaw in held.iter().chain(&wanted) {
        if !(held.contains(flaw) && wanted.contains(flaw)) {
            marks.mark(*flaw, EventEffect::Concerns);
        }
    }
    marks.rerooted(held.first().copied(), wanted.first().copied());
    Ok(true)
}

impl Detail for NewCandidate {
    const KIND: NodeKind = NodeKind::Candidate;
    fn create(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<()> {
        let path = live(db, self.path)?;
        db.execute(
            "INSERT INTO candidate(node,path) VALUES (?1,?2)",
            params![node, path],
        )?;
        marks.mark(path, EventEffect::Concerns);
        set_chain(db, node, &self.chain, marks)?;
        Ok(())
    }
}
impl DetailEdit for CandidateEdit {
    fn edit(&self, db: &Db, node: NodeId, marks: &mut Marks) -> Result<bool> {
        let mut changed = false;
        if let Some(path) = self.path {
            changed |= set(
                db,
                "UPDATE candidate SET path=?2 WHERE node=?1 AND path!=?2",
                node,
                live(db, path)?.sql(),
            )?;
        }
        if let Some(chain) = &self.chain {
            changed |= set_chain(db, node, chain, marks)?;
        }
        Ok(changed)
    }
}

fn create<D: Detail>(db: &Db, actor: &AgentId, new: &New<D>) -> Result<NodeId> {
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

/// A status set: a premise decided, a flaw judged, or a path marked swept or unswept.
#[derive(Clone, Copy, Debug)]
pub enum StatusChange {
    Premise(PremiseId, PremiseStatus),
    Flaw(FlawId, FlawStatus),
    Path(PathId, PathStatus),
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
        }
    }
}

impl Store {
    /// Create a node or edit one, returning its id: a merged node's edit goes to the node it
    /// was merged into.
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
    /// judged by this event. Marking a path moves its own sweep alone, so an unswept
    /// mark offers the sweep again without re-owing its candidates' work.
    pub fn set_status(&self, actor: &AgentId, change: StatusChange, note: &Text) -> Result<()> {
        self.write(|db| {
            audit(db, actor, EventKind::Status, Some(note), |event| {
                let (node, changed) = match change {
                    StatusChange::Premise(id, status) => {
                        let node: NodeId = live(db, id)?.into();
                        let sql = "SELECT 1 FROM premise_state WHERE node=?1 AND status!=?2";
                        let changed = exists(db, sql, params![node, status])?;
                        let sql = "UPDATE premise SET status=?2,decided=?3 WHERE node=?1";
                        db.execute(sql, params![node, status, event])?;
                        (node, changed)
                    }
                    StatusChange::Flaw(id, status) => {
                        let node: NodeId = live(db, id)?.into();
                        let sql = "SELECT 1 FROM flaw_state WHERE node=?1 AND status!=?2";
                        let changed = exists(db, sql, params![node, status])?;
                        let sql = "UPDATE flaw SET status=?2,judged=?3 WHERE node=?1";
                        db.execute(sql, params![node, status, event])?;
                        (node, changed)
                    }
                    StatusChange::Path(id, status) => {
                        let path = live(db, id)?;
                        let swept = (status == PathStatus::Swept).then_some(event);
                        db.execute(
                            "UPDATE path SET swept=?2 WHERE node=?1",
                            params![path, swept],
                        )?;
                        let mut marks = Marks::default();
                        marks.alone(path);
                        return Ok(((), marks));
                    }
                };
                let effect = match changed {
                    true => EventEffect::Changed,
                    false => EventEffect::Concerns,
                };
                Ok(((), Marks::of(node, effect)))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use skyhook::job::JobState;

    use super::{
        super::tests::{child_agent, fixture, last_event, marks, scenario, text},
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
        let at = json!({"file": "inflate.c", "start_line": 3, "end_line": 4});
        let decoder = upsert(
            json!({"kind": "component", "title": "decoder", "body": "inflate",
            "files": ["inflate.c"]}),
        );
        let tables = upsert(
            json!({"kind": "component", "title": "tables", "body": "huffman",
            "files": ["tables.c"]}),
        );
        let source = upsert(json!({"kind": "source", "title": "socket", "body": "reads",
            "location": at, "trust": "untrusted"}));
        let path = upsert(json!({"kind": "path", "title": "stream", "body": "socket to inflate"}));
        let premise = upsert(
            json!({"kind": "premise", "title": "len checked", "body": "before \
            the copy", "scope": "code"}),
        );
        let flaw = upsert(json!({"kind": "flaw", "title": "copy", "body": "unbounded",
            "component": decoder, "location": at, "cwe": 120}));
        let candidate = upsert(
            json!({"kind": "candidate", "title": "overflow", "body": "heap",
            "path": path, "flaws": [flaw]}),
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
        assert!(matches!(
            parse(
                json!({"kind": "candidate", "title": "t", "body": "b", "path": path, "flaws": []})
            ),
            Err(Error::EmptyChain)
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
        upsert(json!({"kind": "component", "id": tables, "files": ["inflate.c"]}));
        assert_eq!(
            effects(),
            [
                (decoder, EventEffect::Changed),
                (tables, EventEffect::Changed)
            ]
        );
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

    /// Any agent marks a path swept or unswept: the event that swept it stands on the path,
    /// and the mark moves the path alone, not its candidates.
    #[test]
    fn any_agent_marks_a_path_swept_or_unswept() {
        let (_session, store, actor) = fixture();
        let path = scenario(&store, &actor, "decoder").path;
        let (stranger, _) = child_agent(&store, JobState::Running);
        let swept = |status| {
            let change = StatusChange::Path(path, status);
            store
                .set_status(&stranger, change, &text("walked it"))
                .unwrap();
            let NodeView::Path(view) = store.node(path.into()).unwrap() else {
                panic!("a path")
            };
            view.swept
        };
        assert_eq!(swept(PathStatus::Swept), Some(last_event(&store)));
        assert_eq!(
            marks(&store, last_event(&store)),
            [(path.into(), EventEffect::Standing)]
        );
        assert_eq!(swept(PathStatus::Unswept), None);
    }
}
