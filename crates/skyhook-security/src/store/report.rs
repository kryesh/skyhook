//! Complete report inputs, findings and their derived standing. One finding reports one
//! root flaw: the scenarios its group leads to, headed by the demonstrated one, keyed by
//! that set. Report files belong to the host; this module never accesses them.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::{
    db::{Db, enum_column, params},
    media::BlobDigest,
};

use super::*;

#[derive(Clone, Debug, Serialize)]
pub struct ReportContent {
    pub run: Run,
    /// The root flaw every scenario of the finding traces back to.
    pub root: FlawRecord,
    /// The scenario the finding leads with: the most severe demonstrated one, otherwise the
    /// most severe.
    pub headline: ReportScenario,
    /// Each of the headline's member flaws as recorded, in scenario order. Its own
    /// exposures carry the routes; other paths' decisions and relations stay out, so
    /// unrelated work never changes the report.
    pub flaws: Vec<FlawRecord>,
    /// The headline's paths as they stand, without their review state.
    pub paths: Vec<ReportPath>,
    pub sources: Vec<Source>,
    /// Every principal the sources name, with its current trust.
    pub principals: Vec<Principal>,
    /// The root's other scenarios with a positive verdict, each with its own conditions.
    pub variants: Vec<ReportScenario>,
}

/// One scenario a finding presents: its assessment, verdict and test as the report states
/// them, the test's artifacts by their recorded references.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Scenario<C> {
    pub candidate: C,
    pub assessment: Assessment,
    pub validation: ValidationAttempt,
    /// Its current test, a failed one included.
    pub poc: Option<PocAttempt>,
}

/// A scenario as a report renders it. The candidate's status is the state the publication
/// presents (validated or noted), never `reported` itself, and its confidence is
/// `confirmed`, or `demonstrated` by its test.
pub type ReportScenario = Scenario<CandidateDetails>;

/// A scenario as a finding stored it.
pub type PublishedScenario = Scenario<CandidateId>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportPath {
    pub id: PathId,
    pub description: String,
    pub sources: Vec<SourceId>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReportSnapshot {
    pub key: ReportKey,
    pub content: ReportContent,
}

/// The identity of a report's generation: the set of scenarios it publishes, each at its
/// assessment, verdict and test ([`ScenarioKey`]), which is also what a publication must
/// still match to stand. Other report content does not key it.
pub type ReportKey = BlobDigest;

/// One published scenario as a generation is keyed by and `finding_candidate` stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(super) struct ScenarioKey {
    pub(super) candidate: CandidateId,
    pub(super) assessment: AssessmentId,
    pub(super) validation: ValidationId,
    pub(super) poc: Option<PocId>,
}

/// A root's report generation as its group stands now: the scenarios it would publish, the
/// headline first, and the key they digest to. The report queue's basis, a finding's key
/// and a publication's freshness all name it; rendering reads the records it names.
#[derive(Clone, Debug)]
pub struct Generation {
    pub key: ReportKey,
    /// Whether a confirmed test demonstrates the headline.
    pub demonstrated: bool,
    /// Each with the state and confidence the publication presents it at.
    scenarios: Vec<(ScenarioKey, Status, ConfidenceTier)>,
}

impl Generation {
    pub(super) fn keys(&self) -> impl Iterator<Item = ScenarioKey> + '_ {
        self.scenarios.iter().map(|(key, ..)| *key)
    }
}

/// The generation of each of `roots` with a reportable scenario, read at once.
pub(super) fn generations(db: &Db, roots: &[FlawId]) -> Result<BTreeMap<FlawId, Generation>> {
    let mut found = BTreeMap::<FlawId, Generation>::new();
    for (root, key, state, confidence) in rows(
        db,
        "SELECT g.root,g.candidate,g.assessment,g.validation,g.poc,g.state,g.confidence FROM \
         json_each(?1) j JOIN group_scenario g ON g.root=j.value ORDER BY \
         g.root,g.confidence='demonstrated' DESC,g.rank DESC,g.candidate",
        params![json_list(roots)?],
        |row| {
            Ok((
                FlawId(row.get(0)?),
                ScenarioKey::of_columns(row, 1)?,
                enum_column::<Status>(row, 5)?,
                enum_column::<ConfidenceTier>(row, 6)?,
            ))
        },
    )? {
        (found.entry(root))
            .or_insert_with(|| Generation {
                key: BlobDigest::of(&[]),
                demonstrated: confidence == ConfidenceTier::Demonstrated,
                scenarios: Vec::new(),
            })
            .scenarios
            .push((key, state, confidence));
    }
    for generation in found.values_mut() {
        generation.key = digest(generation.keys().collect())?;
    }
    Ok(found)
}

/// The key a set of published scenarios digests to, whatever their order.
fn digest(mut keys: Vec<ScenarioKey>) -> Result<ReportKey> {
    keys.sort();
    let bytes =
        serde_json::to_vec(&keys).map_err(|error| skyhook::db::corrupt(error.to_string()))?;
    Ok(BlobDigest::of(&bytes))
}

/// `root`'s generation, refused when no scenario of its group is reportable.
pub(super) fn generation(db: &Db, root: FlawId) -> Result<Generation> {
    (generations(db, &[root])?.remove(&root)).ok_or(Error::NothingToReport { root })
}

impl ScenarioKey {
    /// The key whose candidate, assessment, verdict and test columns start at `offset`.
    fn of_columns(row: &Row, offset: i32) -> Result<Self> {
        Ok(Self {
            candidate: CandidateId(row.get(offset)?),
            assessment: AssessmentId(row.get(offset + 1)?),
            validation: ValidationId(row.get(offset + 2)?),
            poc: row.get::<Option<i64>>(offset + 3)?.map(PocId),
        })
    }

    /// The records the scenario is published at, as a report renders them.
    fn evidence(self, db: &Db) -> Result<PublishedScenario> {
        Ok(Scenario {
            candidate: self.candidate,
            assessment: candidates::assessment(db, self.assessment)?,
            validation: workflow::validation(db, self.validation)?,
            poc: self.poc.map(|poc| workflow::poc(db, poc)).transpose()?,
        })
    }
}

/// A root's latest finding with the scenarios it publishes, whatever its status, and the
/// write-up it was populated with, in the shape `populate` takes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PublishedFinding {
    pub finding: Finding,
    /// The headline first.
    pub scenarios: Vec<PublishedScenario>,
    /// Never shortened: a reader checks what it states.
    #[schemars(transform = skyhook::tool::complete)]
    pub write_up: ReportInput,
}

/// Finding-candidate row `$fc` still presents candidate standing `$st` as it stands now: the
/// same assessment, verdict and test, the tuple a [`ReportKey`] digests. The
/// `candidate_standing` view spells the same comparison.
macro_rules! as_published {
    ($fc:literal, $st:literal) => {
        concat!(
            $fc,
            ".assessment=",
            $st,
            ".assessment AND ",
            $fc,
            ".validation=",
            $st,
            ".validation AND ",
            $fc,
            ".poc IS ",
            $st,
            ".poc"
        )
    };
}
pub(super) use as_published;

/// Finding `$f` publishes exactly its root group's reportable scenarios as they stand.
macro_rules! fresh {
    ($f:literal) => {
        concat!(
            "(NOT EXISTS (SELECT 1 FROM group_scenario g WHERE g.root=",
            $f,
            ".root AND NOT EXISTS (SELECT 1 FROM finding_candidate fc WHERE fc.finding=",
            $f,
            ".id AND fc.candidate=g.candidate AND ",
            report::as_published!("fc", "g"),
            ")) AND NOT EXISTS (SELECT 1 FROM finding_candidate fc WHERE fc.finding=",
            $f,
            ".id AND NOT EXISTS (SELECT 1 FROM group_scenario g WHERE g.root=",
            $f,
            ".root AND g.candidate=fc.candidate AND ",
            report::as_published!("fc", "g"),
            ")))"
        )
    };
}
pub(super) use fresh;

/// Finding `$f` is its root's latest.
macro_rules! latest_finding {
    ($f:literal) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM finding n WHERE n.root=",
            $f,
            ".root AND n.id>",
            $f,
            ".id)"
        )
    };
}
pub(super) use latest_finding;

/// Where a finding stands against its root's group now. Only a current finding is in the
/// session's `reports/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FindingStatus {
    /// The root's latest, publishing exactly its group's reportable scenarios as they stand.
    Current,
    /// The root's latest, while its group's reportable scenarios differ from what it
    /// publishes: the root owes a new report.
    Stale,
    /// A later finding of the root replaced it.
    Superseded { by: FindingId },
    /// The root's latest, with no scenario of its group reportable any longer.
    Withdrawn,
}

impl std::fmt::Display for FindingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Current => f.write_str("current"),
            Self::Stale => f.write_str("stale"),
            Self::Superseded { by } => write!(f, "superseded by #{by}"),
            Self::Withdrawn => f.write_str("withdrawn"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Finding {
    pub id: FindingId,
    pub root: FlawId,
    pub key: ReportKey,
    pub headline: CandidateId,
    /// Every scenario it reports, the headline first.
    pub candidates: Vec<CandidateId>,
    pub severity: Severity,
    /// The headline's as reported: `demonstrated` or `confirmed`.
    pub confidence: ConfidenceTier,
    #[serde(flatten)]
    pub status: FindingStatus,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

/// A current finding with what its report renders: the evidence it publishes as it stands,
/// the prose its reporter wrote and the artifact copies it states.
#[derive(Clone, Debug)]
pub struct CurrentReport {
    pub finding: Finding,
    pub snapshot: ReportSnapshot,
    pub input: ReportInput,
    pub copies: Vec<ArtifactCopy>,
}

/// Findings read as `f`, decoded by [`finding_row`]; a condition follows.
macro_rules! finding_select {
    () => {
        concat!(
            "SELECT f.id,f.root,f.headline,f.at,",
            actor_columns!(),
            ",ifnull((SELECT 'demonstrated' FROM finding_candidate x JOIN poc p ON p.id=x.poc \
             WHERE x.finding=f.id AND x.candidate=f.headline AND p.result='confirmed'),'confirmed'),\
             (SELECT s.severity FROM finding_candidate \
             x JOIN assessment s ON s.id=x.assessment JOIN severity_kind k ON k.name=s.severity \
             WHERE x.finding=f.id ORDER BY k.rank DESC LIMIT 1),\
             (SELECT min(n.id) FROM finding n WHERE n.root=f.root AND n.id>f.id),",
            queues::group_reportable!("f.root"),
            ",",
            fresh!("f"),
            " FROM finding f JOIN actor a ON a.id=f.actor WHERE "
        )
    };
}

fn finding_row(row: &Row) -> Result<Finding> {
    let status = match (
        row.get::<Option<i64>>(7)?,
        row.get::<i64>(8)? != 0,
        row.get::<i64>(9)? != 0,
    ) {
        (Some(by), _, _) => FindingStatus::Superseded { by: FindingId(by) },
        (None, false, _) => FindingStatus::Withdrawn,
        (None, true, false) => FindingStatus::Stale,
        (None, true, true) => FindingStatus::Current,
    };
    Ok(Finding {
        id: FindingId(row.get(0)?),
        root: FlawId(row.get(1)?),
        key: BlobDigest::of(&[]),
        headline: CandidateId(row.get(2)?),
        candidates: Vec::new(),
        at: time(row.get(3)?)?,
        actor: actor_column(row, 4)?,
        confidence: enum_column(row, 5)?,
        severity: enum_column(row, 6)?,
        status,
    })
}

/// Findings with the scenarios each publishes, which its key digests.
fn findings(db: &Db, sql: &'static str, params: Vec<libsql::Value>) -> Result<Vec<Finding>> {
    rows(db, sql, params, finding_row)?
        .into_iter()
        .map(|mut finding| {
            let keys = rows(
                db,
                "SELECT candidate,assessment,validation,poc FROM finding_candidate WHERE \
                 finding=?1 ORDER BY candidate!=?2,candidate",
                params![finding.id.0, finding.headline.0],
                |row| ScenarioKey::of_columns(row, 0),
            )?;
            finding.candidates = keys.iter().map(|key| key.candidate).collect();
            finding.key = digest(keys)?;
            Ok(finding)
        })
        .collect()
}

/// The newest `limit` findings.
pub(super) fn recent(db: &Db, limit: usize) -> Result<Vec<Finding>> {
    findings(
        db,
        concat!(finding_select!(), "1 ORDER BY f.id DESC LIMIT ?1"),
        params![limit],
    )
}

/// The latest finding reporting `candidate`.
pub(super) fn latest_of(db: &Db, candidate: CandidateId) -> Result<Option<Finding>> {
    Ok(findings(
        db,
        concat!(
            finding_select!(),
            "EXISTS (SELECT 1 FROM finding_candidate x WHERE x.finding=f.id AND x.candidate=?1) \
             ORDER BY f.id DESC LIMIT 1"
        ),
        params![candidate.0],
    )?
    .pop())
}

/// The root's latest finding, whatever generation it publishes.
fn latest(db: &Db, root: FlawId) -> Result<Option<Finding>> {
    Ok(findings(
        db,
        concat!(finding_select!(), "f.root=?1 AND ", latest_finding!("f")),
        params![root.0],
    )?
    .pop())
}

/// The root's latest finding, when it publishes generation `key`.
fn finding_of(db: &Db, root: FlawId, key: &ReportKey) -> Result<Option<Finding>> {
    Ok(latest(db, root)?.filter(|finding| finding.key == *key))
}

impl Store {
    /// `root`'s report generation as its group stands now.
    pub fn report_generation(&self, root: FlawId) -> Result<Generation> {
        self.read(|db| generation(db, root))
    }

    /// The tests `generation` states, the headline's first.
    pub fn report_tests(&self, generation: &Generation) -> Result<Vec<PocAttempt>> {
        self.read(|db| {
            (generation.keys().filter_map(|key| key.poc))
                .map(|poc| workflow::poc(db, poc))
                .collect()
        })
    }

    /// Records `root`'s finding with its reporter's prose while the root's generation is
    /// still `key`, with the artifact copies its tests state. When the root's latest finding
    /// already publishes `key`, its write-up is replaced instead, unless it already reads as
    /// `input`, and the copies it recorded stand. A root has a new finding each time its
    /// reportable scenarios differ from its latest one's.
    pub fn record_finding(
        &self,
        actor: &AgentId,
        input: &WriteUp,
        key: &ReportKey,
        copies: &[ArtifactCopy],
    ) -> Result<Finding> {
        let root = input.root;
        self.write(|db| {
            let generation = generation(db, root)?;
            if generation.key != *key {
                return Err(Error::SnapshotChanged);
            }
            let keys: Vec<ScenarioKey> = generation.keys().collect();
            // Only prose that is written records its reporter.
            let with_prose = |mut leading: Vec<libsql::Value>| -> Result<Vec<libsql::Value>> {
                let (observed, expected, limitations) = match &input.reproduction.outcome {
                    ReproductionOutcome::Observed { observed_result } => {
                        (Some(observed_result), None, None)
                    }
                    ReproductionOutcome::Unobserved {
                        expected_result,
                        limitations,
                    } => (None, Some(expected_result), Some(limitations)),
                };
                leading.extend(params![
                    &input.title,
                    &input.summary,
                    &input.analysis,
                    &input.impact,
                    &input.remediation,
                    observed,
                    expected,
                    limitations,
                    actor_row(db, actor)?,
                    now()
                ]);
                Ok(leading)
            };
            let id = match finding_of(db, root, key)? {
                Some(finding) if authored::input(db, finding.id)? == **input => return Ok(finding),
                Some(finding) => {
                    db.execute(
                        "UPDATE finding SET title=?2,summary=?3,analysis=?4,impact=?5,remediation=?6,observed_result=?7,expected_result=?8,limitations=?9,actor=?10,at=?11 WHERE id=?1",
                        with_prose(params![finding.id.0])?,
                    )?;
                    authored::clear(db, finding.id)?;
                    let copies = authored::copies(db, finding.id)?;
                    authored::insert(db, finding.id, &keys, input, &copies)?;
                    finding.id
                }
                None => {
                    let id = FindingId(db.insert(
                        "INSERT INTO finding(root,headline,title,summary,analysis,impact,remediation,observed_result,expected_result,limitations,actor,at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                        with_prose(params![root.0, keys[0].candidate.0])?,
                    )?);
                    for key in &keys {
                        db.execute(
                            "INSERT INTO finding_candidate(finding,candidate,assessment,validation,poc) VALUES (?1,?2,?3,?4,?5)",
                            params![
                                id.0,
                                key.candidate.0,
                                key.assessment.0,
                                key.validation.0,
                                key.poc.map(|poc| poc.0)
                            ],
                        )?;
                    }
                    authored::insert_copies(db, id, &keys, copies)?;
                    authored::insert(db, id, &keys, input, copies)?;
                    id
                }
            };
            let finding = findings(db, concat!(finding_select!(), "f.id=?1"), params![id.0])?
                .pop()
                .ok_or_else(|| corrupt("a recorded finding is missing"))?;
            let subjects: Vec<Subject> = (finding.candidates.iter().copied())
                .map(Subject::Candidate)
                .chain([Subject::Flaw(root)])
                .collect();
            audit(
                db,
                actor,
                EventKind::Reported,
                Detail::Record(EventRecord::Finding(finding.id)),
                &subjects,
            )?;
            Ok(finding)
        })
    }

    /// The root's latest finding, when it publishes generation `key`.
    pub fn finding_of(&self, root: FlawId, key: &ReportKey) -> Result<Option<Finding>> {
        self.read(|db| finding_of(db, root, key))
    }

    /// The finding [`Self::record_finding`] leaves once it commits, when the store holds it:
    /// the root's latest finding publishes `key` and reads as `input`. What settles a record
    /// whose commit is unknown.
    pub fn recorded_finding(&self, input: &WriteUp, key: &ReportKey) -> Result<Option<Finding>> {
        self.read(|db| match finding_of(db, input.root, key)? {
            Some(finding) if authored::input(db, finding.id)? == **input => Ok(Some(finding)),
            _ => Ok(None),
        })
    }

    /// The root's latest finding with what it publishes and its write-up.
    pub fn finding(&self, root: FlawId) -> Result<PublishedFinding> {
        self.read(|db| {
            let finding = latest(db, root)?.ok_or_else(|| Record::Finding.missing())?;
            Ok(PublishedFinding {
                scenarios: published(db, finding.id)?,
                write_up: authored::input(db, finding.id)?,
                finding,
            })
        })
    }

    /// Every current finding with what its report renders, by root, from one snapshot.
    pub fn current_reports(&self) -> Result<Vec<CurrentReport>> {
        self.read(|db| {
            let latest = findings(
                db,
                concat!(finding_select!(), latest_finding!("f"), " ORDER BY f.root"),
                Vec::new(),
            )?;
            (latest.into_iter())
                .filter(|finding| finding.status == FindingStatus::Current)
                .map(|finding| {
                    Ok(CurrentReport {
                        snapshot: report_snapshot(db, finding.root)?,
                        input: authored::input(db, finding.id)?,
                        copies: authored::copies(db, finding.id)?,
                        finding,
                    })
                })
                .collect()
        })
    }

    /// Findings, or those reporting one candidate.
    pub fn findings(&self, candidate: Option<CandidateId>, page: Page) -> Result<Listing<Finding>> {
        self.read(|db| {
            let findings = findings(
                db,
                concat!(
                    finding_select!(),
                    "f.id>?1 AND (?3 IS NULL OR EXISTS (SELECT 1 FROM finding_candidate x WHERE \
                     x.finding=f.id AND x.candidate=?3)) ORDER BY f.id LIMIT ?2"
                ),
                params![page.after, page.limit(), candidate.map(|id| id.0)],
            )?;
            Ok(Listing::keyset(findings, page))
        })
    }
}

/// The snapshot of `root`'s report: every scenario of its group with a positive verdict,
/// whatever its test showed, the demonstrated and most severe first. Publishing is not new
/// research evidence: each scenario is presented by the state its review settles on, so the
/// publication itself never changes the snapshot.
pub(super) fn report_snapshot(db: &Db, root: FlawId) -> Result<ReportSnapshot> {
    flaws::live(db, root)?;
    let generation = generation(db, root)?;
    let mut scenarios = (generation.scenarios.iter())
        .map(|&(key, state, confidence)| scenario(db, key, state, confidence));
    let headline = scenarios.next().ok_or(Error::NothingToReport { root })??;
    let variants = scenarios.collect::<Result<Vec<_>>>()?;
    let head = &headline.candidate;
    let flaws = (head.members.iter())
        .map(|member| flaws::record(db, member.flaw.id))
        .collect::<Result<Vec<_>>>()?;
    let paths = (head.paths.iter())
        .map(|id| {
            Ok(ReportPath {
                id: *id,
                description: one(db, "SELECT description FROM path WHERE id=?1", *id, |row| {
                    Ok(row.get(0)?)
                })?,
                sources: rows(
                    db,
                    "SELECT source FROM path_source WHERE path=?1 ORDER BY source",
                    params![id.0],
                    |row| Ok(SourceId(row.get(0)?)),
                )?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let sources = (head.sources.iter())
        .map(|id| scope::source(db, *id))
        .collect::<Result<Vec<_>>>()?;
    let principals = (sources.iter())
        .flat_map(|source| source.principals.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|id| scope::principal(db, id))
        .collect::<Result<Vec<_>>>()?;
    let content = ReportContent {
        run: runs::run(db)?,
        root: flaws::record(db, root)?,
        headline,
        flaws,
        paths,
        sources,
        principals,
        variants,
    };
    Ok(ReportSnapshot {
        key: generation.key,
        content,
    })
}

fn scenario(
    db: &Db,
    key: ScenarioKey,
    state: Status,
    confidence: ConfidenceTier,
) -> Result<ReportScenario> {
    let published = key.evidence(db)?;
    let mut candidate = candidates::details(db, key.candidate)?;
    candidate.candidate.status = state;
    candidate.candidate.confidence = Some(confidence);
    Ok(Scenario {
        candidate,
        assessment: published.assessment,
        validation: published.validation,
        poc: published.poc,
    })
}

/// Every scenario finding `id` publishes, in the snapshot's order: the demonstrated and most
/// severe first.
fn published(db: &Db, id: FindingId) -> Result<Vec<PublishedScenario>> {
    rows(
        db,
        "SELECT fc.candidate,fc.assessment,fc.validation,fc.poc FROM finding_candidate fc JOIN \
         assessment s ON s.id=fc.assessment JOIN severity_kind k ON k.name=s.severity LEFT JOIN \
         poc p ON p.id=fc.poc WHERE fc.finding=?1 ORDER BY p.result IS 'confirmed' DESC,k.rank \
         DESC,fc.candidate",
        params![id.0],
        |row| ScenarioKey::of_columns(row, 0),
    )?
    .into_iter()
    .map(|key| key.evidence(db))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{contributes, fixture, path, report_input, text, validated};
    use super::*;

    /// Only the published scenarios key a generation: a member flaw's decision along a path
    /// outside the candidate leaves the key alone.
    #[test]
    fn decisions_outside_the_candidate_leave_the_report_key() {
        let (_session, store, actor) = fixture();
        let candidate = validated(&store, &actor);
        let flaw = store.candidate(candidate).unwrap().members[0].flaw.id;
        let before = store.report_generation(flaw).unwrap().key;
        let elsewhere = path(&store, &actor, "elsewhere", &[]);
        contributes(&store, &actor, flaw, elsewhere);
        assert_eq!(store.report_generation(flaw).unwrap().key, before);
    }

    /// Populating a finding with the write-up it already has records nothing of the
    /// reporter, not even its actor.
    #[test]
    fn an_unchanged_write_up_records_no_reporter() {
        let (_session, store, actor) = fixture();
        let candidate = validated(&store, &actor);
        let root = store.candidate(candidate).unwrap().members[0].flaw.id;
        let key = store.report_generation(root).unwrap().key;
        let first = store
            .record_finding(&actor, &report_input(root), &key, &[])
            .unwrap();
        let actors = || store.read(|db| count(db, "SELECT count(*) FROM actor", params![]));
        let before = actors().unwrap();
        let reporter = skyhook::identity::AgentId::root(store.id()).child(7);
        let again = store
            .record_finding(&reporter, &report_input(root), &key, &[])
            .unwrap();
        assert_eq!((again.id, again.actor), (first.id, first.actor));
        assert_eq!(actors().unwrap(), before);
    }

    /// A record whose commit is unknown is settled once the root's latest finding publishes
    /// its generation with its write-up.
    #[test]
    fn a_record_settles_on_its_write_up() {
        let (_session, store, actor) = fixture();
        let candidate = validated(&store, &actor);
        let root = store.candidate(candidate).unwrap().members[0].flaw.id;
        let key = store.report_generation(root).unwrap().key;
        let input = report_input(root);
        let recorded = |input: &WriteUp| {
            (store.recorded_finding(input, &key).unwrap()).map(|finding| finding.id)
        };
        assert_eq!(recorded(&input), None);
        let first = store.record_finding(&actor, &input, &key, &[]).unwrap().id;
        assert_eq!(recorded(&input), Some(first));
        let mut reworded = (*input).clone();
        reworded.title = text("reworded");
        assert_eq!(recorded(&reworded.canonical()), None);
    }
}
