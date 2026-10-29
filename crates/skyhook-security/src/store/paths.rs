//! Data paths: sources that share one interpretation up to their handoff into
//! implementation. A path is reviewed once against the contracts of the components it
//! reaches; its decisions about flaws are keyed by it, not by each source.

use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, params};

use super::*;

/// One component a path reaches.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reach {
    pub component: ComponentId,
    /// How the path gets there.
    pub note: Text,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPath {
    pub description: Text,
    /// Sources that are interpreted identically up to the handoff; at least one.
    pub sources: Vec<SourceId>,
    #[serde(default)]
    pub reaches: Vec<Reach>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PathReach {
    pub component: ComponentId,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Path {
    pub id: PathId,
    pub description: String,
    /// Bumped by every membership change; decisions record the revision they saw.
    pub revision: u32,
    pub sources: Vec<SourceId>,
    pub reaches: Vec<PathReach>,
    pub coverage: PathCoverage,
    /// Stretches of the path left unexamined.
    pub deferred: Vec<Deferred>,
}

/// What one path's review still has to decide or place.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct PathWork {
    /// Flaws without this path's standing decision.
    pub decisions: Vec<FlawId>,
    /// `contributes` decisions whose flaw is in no live candidate of this path.
    pub unplaced: Vec<ExposureId>,
}

/// No standing decision by path `$path` about flaw `f`.
macro_rules! undecided {
    ($path:literal) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM exposure_decision e WHERE e.flaw=f.id AND e.path=",
            $path,
            " AND e.standing)"
        )
    };
}
pub(super) use undecided;

/// A candidate `$candidate` neither merged nor rejected as disproved.
macro_rules! live_candidate {
    ($candidate:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM candidate_standing ls WHERE ls.candidate=",
            $candidate,
            " AND ls.status!='merged' AND NOT (ls.status='rejected' AND ls.reason='disproved'))"
        )
    };
}
pub(super) use live_candidate;

/// The current flaws path `$path` owes a decision about, a row per reason: those a reach or a
/// candidate implies, and every one decided before whose latest decision no longer stands,
/// wherever its flaw now lives. Each branch is filtered to the path, so one path's work never
/// computes every other path's.
macro_rules! undecided_on {
    ($path:literal) => {
        concat!(
            "SELECT f.id AS flaw FROM path_reach r JOIN flaw_state f ON f.component=r.component \
             WHERE r.path=",
            $path,
            " AND f.current AND ",
            paths::undecided!($path),
            " UNION ALL SELECT f.id FROM candidate_path cp JOIN candidate c ON c.id=cp.candidate \
             AND c.merged_into IS NULL JOIN candidate_member m ON m.candidate=c.id JOIN \
             flaw_state f ON f.id=m.flaw WHERE cp.path=",
            $path,
            " AND f.current AND ",
            paths::undecided!($path),
            " UNION ALL SELECT f.id FROM exposure_decision e JOIN flaw_state f ON f.id=e.flaw \
             WHERE e.path=",
            $path,
            " AND NOT e.standing AND f.current"
        )
    };
}
pub(super) use undecided_on;

/// A path `p`'s coverage, decoded as [`PathCoverage`].
macro_rules! path_coverage {
    () => {
        concat!(
            "CASE WHEN NOT EXISTS (SELECT 1 FROM path_reach WHERE path=p.id) THEN 'unreached' \
             WHEN EXISTS (",
            paths::undecided_on!("p.id"),
            ") THEN 'unchecked' ELSE 'checked' END"
        )
    };
}
pub(super) use path_coverage;

fn path_row(row: &Row) -> Result<Path> {
    Ok(Path {
        id: PathId(row.get(0)?),
        description: row.get(1)?,
        revision: row.get::<i64>(2)? as u32,
        coverage: enum_column(row, 3)?,
        sources: Vec::new(),
        reaches: Vec::new(),
        deferred: Vec::new(),
    })
}

fn hydrate(db: &Db, mut path: Path) -> Result<Path> {
    path.sources = rows(
        db,
        "SELECT source FROM path_source WHERE path=?1 ORDER BY source",
        params![path.id.0],
        |row| Ok(SourceId(row.get(0)?)),
    )?;
    path.reaches = rows(
        db,
        "SELECT component,note FROM path_reach WHERE path=?1 ORDER BY component",
        params![path.id.0],
        |row| {
            Ok(PathReach {
                component: ComponentId(row.get(0)?),
                note: row.get(1)?,
            })
        },
    )?;
    path.deferred = components::deferred_of(db, DeferredSubject::Path(path.id))?;
    Ok(path)
}

macro_rules! path_select {
    () => {
        concat!(
            "SELECT p.id,p.description,p.revision,",
            paths::path_coverage!(),
            " FROM path p WHERE "
        )
    };
}

pub(super) fn path(db: &Db, id: PathId) -> Result<Path> {
    let path = one(db, concat!(path_select!(), "p.id=?1"), id, path_row)?;
    hydrate(db, path)
}

/// The path's revision, refused for an unknown path.
pub(super) fn revision(db: &Db, id: PathId) -> Result<u32> {
    one(db, "SELECT revision FROM path WHERE id=?1", id, |row| {
        Ok(row.get::<i64>(0)? as u32)
    })
}

/// Record reaches, returning whether any reach or note changed; a component the path now
/// reaches is planned a sweep when its sweeps do not cover it.
fn insert_reaches(db: &Db, actor: &AgentId, path: PathId, reaches: &[Reach]) -> Result<bool> {
    if reaches.len() > MAX_ATTACHMENTS {
        return Err(Error::Limit(Bound::Attachments));
    }
    let mut changed = false;
    for reach in reaches {
        scope::known(db, reach.component)?;
        changed |= db.execute(
            "INSERT INTO path_reach(path,component,note) VALUES (?1,?2,?3) ON CONFLICT DO UPDATE \
             SET note=excluded.note WHERE note IS NOT excluded.note",
            params![path.0, reach.component.0, &reach.note],
        )? > 0;
        plan::sweep_owed(db, actor, reach.component)?;
    }
    Ok(changed)
}

/// Whether a source joined or left: a membership change, which bumps the path and every
/// candidate it carries.
fn change_sources(db: &Db, path: PathId, add: &[SourceId], remove: &[SourceId]) -> Result<bool> {
    if add.len() + remove.len() > MAX_ATTACHMENTS {
        return Err(Error::Limit(Bound::Attachments));
    }
    let mut changed = false;
    for source in add {
        scope::known(db, *source)?;
        changed |= db.execute(
            "INSERT INTO path_source(path,source) VALUES (?1,?2) ON CONFLICT DO NOTHING",
            params![path.0, source.0],
        )? > 0;
    }
    for source in remove {
        changed |= db.execute(
            "DELETE FROM path_source WHERE path=?1 AND source=?2",
            params![path.0, source.0],
        )? > 0;
    }
    if !exists(
        db,
        "SELECT 1 FROM path_source WHERE path=?1",
        params![path.0],
    )? {
        return Err(Error::EmptyPath);
    }
    Ok(changed)
}

fn updated(db: &Db, actor: &AgentId, path: PathId) -> Result<()> {
    audit(
        db,
        actor,
        EventKind::Updated,
        Detail::Record(EventRecord::Path(path)),
        &[],
    )
    .map(drop)
}

impl Store {
    /// Records a path with its sources and the components it reaches, planning its review
    /// and a sweep of every component no sweep is planned for yet.
    pub fn add_path(&self, actor: &AgentId, input: &NewPath) -> Result<PathId> {
        self.write(|db| {
            let id = PathId(db.insert(
                "INSERT INTO path(description) VALUES (?1)",
                params![&input.description],
            )?);
            change_sources(db, id, &input.sources, &[])?;
            audit(
                db,
                actor,
                EventKind::Recorded,
                Detail::Record(EventRecord::Path(id)),
                &[],
            )?;
            plan::owed(db, actor, &UnitWork::PathReview { path: id })?;
            insert_reaches(db, actor, id, &input.reaches)?;
            Ok(id)
        })
    }

    /// Sources join or leave the path; a change bumps its revision, so every decision about
    /// it is owed again, plans its review, and reopens the candidates it carries. A restated
    /// membership changes nothing.
    pub fn change_path_sources(
        &self,
        actor: &AgentId,
        path: PathId,
        add: &[SourceId],
        remove: &[SourceId],
    ) -> Result<()> {
        self.write(|db| {
            revision(db, path)?;
            if !change_sources(db, path, add, remove)? {
                return Ok(());
            }
            db.execute(
                "UPDATE path SET revision=revision+1 WHERE id=?1",
                params![path.0],
            )?;
            let carried = rows(
                db,
                "SELECT candidate FROM candidate_path WHERE path=?1",
                params![path.0],
                |row| Ok(CandidateId(row.get(0)?)),
            )?;
            flaws::touch_candidates(db, &carried)?;
            plan::review_owed(db, actor, path)?;
            updated(db, actor, path)
        })
    }

    /// More components the path reaches, or a changed note; restating them changes nothing.
    pub fn record_reach(&self, actor: &AgentId, path: PathId, reaches: &[Reach]) -> Result<()> {
        self.write(|db| {
            revision(db, path)?;
            match insert_reaches(db, actor, path, reaches)? {
                true => updated(db, actor, path),
                false => Ok(()),
            }
        })
    }

    pub fn path(&self, id: PathId) -> Result<Path> {
        self.read(|db| path(db, id))
    }

    pub fn paths(&self, page: Page) -> Result<Listing<Path>> {
        self.read(|db| {
            let paths = rows(
                db,
                concat!(path_select!(), "p.id>?1 ORDER BY p.id LIMIT ?2"),
                params![page.after, page.limit()],
                path_row,
            )?
            .into_iter()
            .map(|path| hydrate(db, path))
            .collect::<Result<_>>()?;
            Ok(Listing::keyset(paths, page))
        })
    }

    /// What `path`'s review still has to decide or place.
    pub fn path_work(&self, path: PathId) -> Result<PathWork> {
        self.read(|db| {
            revision(db, path)?;
            let ids =
                |sql: &'static str| rows(db, sql, params![path.0], |row| Ok(row.get::<i64>(0)?));
            Ok(PathWork {
                decisions: ids(concat!(
                    "SELECT DISTINCT flaw FROM (",
                    paths::undecided_on!("?1"),
                    ") ORDER BY flaw"
                ))?
                .into_iter()
                .map(FlawId)
                .collect(),
                unplaced: ids(concat!(
                    "SELECT e.id FROM exposure_decision e JOIN flaw_state f ON f.id=e.flaw WHERE \
                     e.path=?1 AND e.standing AND e.outcome='contributes' AND f.current AND NOT \
                     EXISTS (SELECT 1 FROM candidate_member m JOIN candidate_path cp ON \
                     cp.candidate=m.candidate AND cp.path=e.path WHERE m.flaw=f.id AND ",
                    paths::live_candidate!("m.candidate"),
                    ") ORDER BY e.id"
                ))?
                .into_iter()
                .map(ExposureId)
                .collect(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            assess, component, contradiction, contributes, fixture, member, new_candidate,
            new_flaw, path, shortlisted, source, status, text, verdict,
        },
        *,
    };

    /// One path's decisions serve every source on it and no other path; a membership
    /// change owes them again, with the candidates the path carries.
    #[test]
    fn a_path_keys_decisions_and_a_membership_change_reopens_them() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let stream = path(&store, &actor, "stream", &[decoder]);
        let chunked = source(&store, &actor, "chunked", Trust::Unauthenticated);
        store
            .change_path_sources(&actor, stream, &[chunked], &[])
            .unwrap();
        let flaw = store
            .add_flaw(&actor, &new_flaw(decoder, "tail copy"))
            .unwrap();
        assert_eq!(store.path_work(stream).unwrap().decisions, [flaw]);
        contributes(&store, &actor, flaw, stream);
        assert_eq!(store.path(stream).unwrap().coverage, PathCoverage::Checked);
        let one_shot = path(&store, &actor, "one-shot", &[decoder]);
        assert_eq!(
            store.path_work(one_shot).unwrap().decisions,
            [flaw],
            "another path's decisions never settle this one"
        );

        let candidate = new_candidate(FlawChoice::Existing(flaw), vec![stream]);
        let id = store.add_candidate(&actor, &candidate).unwrap();
        assess(&store, &actor, id);
        assert_eq!(status(&store, id), Status::Shortlisted);
        store
            .change_path_sources(&actor, stream, &[], &[chunked])
            .unwrap();
        assert_eq!(store.path(stream).unwrap().revision, 3);
        assert_eq!(store.path_work(stream).unwrap().decisions, [flaw]);
        assert_eq!(status(&store, id), Status::Identified);
        let only = store.path(stream).unwrap().sources;
        assert!(matches!(
            store.change_path_sources(&actor, stream, &[], &only),
            Err(Error::EmptyPath)
        ));
    }

    #[test]
    fn a_disproved_formulation_leaves_its_contribution_unplaced() {
        let (_directory, store, actor) = fixture();
        let id = shortlisted(&store, &actor);
        let (flaw, path) = member(&store, id);
        verdict(
            &store,
            &actor,
            id,
            Verdict::PolicyExcluded { deployment: None },
        );
        assert!(store.path_work(path).unwrap().unplaced.is_empty());
        store
            .add_scope(&actor, &text("policy revised"), &[])
            .unwrap();

        verdict(&store, &actor, id, Verdict::Disproved);
        let work = store.path_work(path).unwrap();
        assert_eq!(
            work.unplaced.len(),
            1,
            "the contributing exposure has no live candidate"
        );
        assert!(work.decisions.is_empty());

        // The alternative reading becomes a candidate of its own and settles placement.
        let alternative = new_candidate(FlawChoice::Existing(flaw), vec![path]);
        store.add_candidate(&actor, &alternative).unwrap();
        let work = store.path_work(path).unwrap();
        assert!(work.unplaced.is_empty());
    }

    #[test]
    fn contradicting_a_dismissal_owes_its_pair_a_new_decision() {
        let (_directory, store, actor) = fixture();
        let component = component(&store, &actor, "decoder");
        let path = path(&store, &actor, "entry", &[component]);
        let flaw = store
            .add_flaw(&actor, &new_flaw(component, "unbounded tail copy"))
            .unwrap();
        let dismiss = |note: &str| {
            store
                .record_exposure(
                    &actor,
                    &NewExposure {
                        flaw,
                        path,
                        outcome: ExposureOutcome::NoConsequence,
                        conditions: text("the tail is at most seven bytes"),
                        note: text(note),
                        route: Vec::new(),
                    },
                )
                .unwrap()
        };
        let exposure = dismiss("bounded by the caller");
        assert!(store.path_work(path).unwrap().decisions.is_empty());
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Exposure(exposure),
                &contradiction("a resumed call leaves more"),
            )
            .unwrap();
        assert_eq!(store.path_work(path).unwrap().decisions, [flaw]);
        dismiss("rechecked: still bounded");
        assert!(store.path_work(path).unwrap().decisions.is_empty());
    }
}
