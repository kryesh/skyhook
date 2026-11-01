//! Each kind's neighbourhood, read in one snapshot.

use libsql::Row;
use skyhook::db::{Db, enum_column, optional_enum_column, params};

use super::{super::*, views::*};

fn ref_row(row: &Row) -> Result<Ref> {
    Ok(Ref {
        id: NodeId(row.get(0)?),
        title: row.get(1)?,
    })
}

/// Premises read as `p` joined to their node `n`, decoded by [`premise_ref`].
macro_rules! premise_refs {
    ($from:expr, $filter:literal) => {
        concat!(
            "SELECT p.node,n.title,p.scope,p.status FROM ",
            $from,
            " JOIN premise_state p ON p.node=x.premise JOIN node n ON n.id=p.node WHERE ",
            $filter,
            " ORDER BY p.node"
        )
    };
}

fn premise_ref(row: &Row) -> Result<PremiseRef> {
    Ok(PremiseRef {
        id: PremiseId(row.get(0)?),
        title: row.get(1)?,
        scope: enum_column(row, 2)?,
        status: enum_column(row, 3)?,
    })
}

fn candidate_ref(row: &Row) -> Result<CandidateRef> {
    Ok(CandidateRef {
        id: CandidateId(row.get(0)?),
        title: row.get(1)?,
        status: enum_column(row, 2)?,
    })
}

pub(super) fn evidence(db: &Db, node: NodeId) -> Result<Vec<EvidenceView>> {
    rows(
        db,
        concat!(
            "SELECT v.id,v.stance,v.note,v.event,v.environment,",
            location_columns!(),
            " FROM evidence v",
            location_joins!("v.location"),
            " WHERE v.node=?1 ORDER BY v.id"
        ),
        params![node],
        |row| {
            Ok(EvidenceView {
                id: EvidenceId(row.get(0)?),
                stance: enum_column(row, 1)?,
                note: row.get(2)?,
                event: EventId(row.get(3)?),
                environment: row.get::<Option<i64>>(4)?.map(EnvironmentId),
                location: location_column(row, 5)?,
            })
        },
    )
}

pub(super) fn steps(db: &Db, path: NodeId) -> Result<Vec<Step>> {
    rows(
        db,
        concat!(
            "SELECT s.position,s.component,n.title,s.note,",
            location_columns!(),
            " FROM path_step s JOIN node n ON n.id=s.component",
            location_joins!("s.location"),
            " WHERE s.path=?1 ORDER BY s.position"
        ),
        params![path],
        |row| {
            Ok(Step {
                position: row.get(0)?,
                component: Ref {
                    id: NodeId(row.get(1)?),
                    title: row.get(2)?,
                },
                note: row.get(3)?,
                location: location_column(row, 4)?,
            })
        },
    )
}

pub(super) fn sources(db: &Db, path: NodeId) -> Result<Vec<SourceRef>> {
    rows(
        db,
        concat!(
            "SELECT s.node,n.title,s.trust,s.status,",
            location_columns!(),
            " FROM path_source x JOIN source s ON s.node=x.source JOIN node n ON n.id=s.node",
            location_joins!("s.location"),
            " WHERE x.path=?1 ORDER BY s.node"
        ),
        params![path],
        |row| {
            Ok(SourceRef {
                id: SourceId(row.get(0)?),
                title: row.get(1)?,
                trust: enum_column(row, 2)?,
                status: enum_column(row, 3)?,
                location: located(row, 4)?,
            })
        },
    )
}

pub(super) fn path_premises(db: &Db, path: NodeId) -> Result<Vec<PremiseRef>> {
    rows(
        db,
        premise_refs!("path_premise x", "x.path=?1"),
        params![path],
        premise_ref,
    )
}

pub(super) fn environments(
    db: &Db,
    sql: &'static str,
    params: Vec<libsql::Value>,
) -> Result<Vec<EnvironmentView>> {
    let mut environments = rows(db, sql, params, |row| {
        Ok(EnvironmentView {
            id: EnvironmentId(row.get(0)?),
            instructions: row.get(1)?,
            candidate: row.get::<Option<i64>>(2)?.map(CandidateId),
            serves: Vec::new(),
            event: EventId(row.get(3)?),
        })
    })?;
    let ids: Vec<i64> = environments
        .iter()
        .map(|environment| environment.id.0)
        .collect();
    let served = rows(
        db,
        "SELECT j.key,ei.interface,ei.address FROM json_each(?1) j JOIN environment_interface ei \
         ON ei.environment=j.value ORDER BY j.key,ei.interface",
        params![json_list(&ids)?],
        |row| {
            let served = Served {
                interface: row.get::<String>(1)?.parse()?,
                address: row.get::<String>(2)?.parse()?,
            };
            Ok((row.get::<i64>(0)? as usize, served))
        },
    )?;
    for (index, served) in served {
        environments[index].serves.push(served);
    }
    Ok(environments)
}

pub(super) fn component(db: &Db, head: Head) -> Result<ComponentView> {
    let id = head.id;
    Ok(ComponentView {
        status: stored(
            db,
            "SELECT status FROM component WHERE node=?1",
            params![id],
            |row| Ok(enum_column(row, 0)?),
        )?,
        files: rows(
            db,
            "SELECT file FROM component_file WHERE component=?1 ORDER BY file",
            params![id],
            |row| Ok(row.get(0)?),
        )?,
        interfaces: rows(
            db,
            "SELECT interface FROM component_interface WHERE component=?1 ORDER BY interface",
            params![id],
            |row| Ok(row.get::<String>(0)?.parse()?),
        )?,
        paths: rows(
            db,
            "SELECT DISTINCT n.id,n.title FROM path_step s JOIN node n ON n.id=s.path WHERE \
             s.component=?1 AND n.merged_into IS NULL ORDER BY n.id",
            params![id],
            ref_row,
        )?,
        flaws: rows(
            db,
            "SELECT f.node,n.title,f.status FROM flaw_state f JOIN node n ON n.id=f.node WHERE \
             f.component=?1 AND n.merged_into IS NULL ORDER BY f.node",
            params![id],
            |row| {
                Ok(FlawRef {
                    id: FlawId(row.get(0)?),
                    title: row.get(1)?,
                    status: enum_column(row, 2)?,
                })
            },
        )?,
        head,
    })
}

pub(super) fn path(db: &Db, head: Head) -> Result<PathView> {
    let id = head.id;
    Ok(PathView {
        steps: steps(db, id)?,
        sources: sources(db, id)?,
        premises: path_premises(db, id)?,
        candidates: rows(
            db,
            "SELECT DISTINCT s.candidate,n.title,s.state FROM candidate_action c JOIN \
             candidate_state s ON s.candidate=c.candidate JOIN node n ON n.id=c.candidate WHERE \
             c.path=?1 AND n.merged_into IS NULL ORDER BY c.candidate",
            params![id],
            candidate_ref,
        )?,
        status: stored(
            db,
            "SELECT status FROM path WHERE node=?1",
            params![id],
            |row| Ok(enum_column(row, 0)?),
        )?,
        head,
    })
}

pub(super) fn source(db: &Db, head: Head) -> Result<SourceView> {
    let id = head.id;
    let held = optional(
        db,
        concat!(
            "SELECT s.trust,s.status,",
            location_columns!(),
            " FROM source s",
            location_joins!("s.location"),
            " WHERE s.node=?1"
        ),
        params![id],
        |row| Ok((enum_column(row, 0)?, enum_column(row, 1)?, located(row, 2)?)),
    )?;
    let live = held
        .map(|(trust, status, location)| -> Result<_> {
            Ok(LiveSource {
                location,
                trust,
                status,
                paths: rows(
                    db,
                    "SELECT n.id,n.title FROM path_source x JOIN node n ON n.id=x.path WHERE \
                     x.source=?1 ORDER BY n.id",
                    params![id],
                    ref_row,
                )?,
            })
        })
        .transpose()?;
    Ok(SourceView { head, live })
}

/// Contradicting evidence on node `$id` came after `$stamp`, the event that last set its
/// status.
macro_rules! disputed {
    ($id:literal, $stamp:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM evidence d WHERE d.node=",
            $id,
            " AND d.stance='contradicts' AND d.event>ifnull(",
            $stamp,
            ",0))"
        )
    };
}
pub(in crate::store) use disputed;

pub(super) fn premise(db: &Db, head: Head) -> Result<PremiseView> {
    let id = head.id;
    let (scope, status, disputed, location) = stored(
        db,
        concat!(
            "SELECT p.scope,p.status,",
            disputed!("p.node", "p.decided"),
            ",",
            location_columns!(),
            " FROM premise_state p",
            location_joins!("p.location"),
            " WHERE p.node=?1"
        ),
        params![id],
        |row| {
            Ok((
                enum_column(row, 0)?,
                enum_column(row, 1)?,
                row.get::<bool>(2)?,
                location_column(row, 3)?,
            ))
        },
    )?;
    Ok(PremiseView {
        scope,
        status,
        disputed,
        location,
        flaws: rows(
            db,
            "SELECT n.id,n.title FROM flaw_premise x JOIN node n ON n.id=x.flaw WHERE x.premise=?1 \
             ORDER BY n.id",
            params![id],
            ref_row,
        )?,
        paths: rows(
            db,
            "SELECT n.id,n.title FROM path_premise x JOIN node n ON n.id=x.path WHERE \
             x.premise=?1 ORDER BY n.id",
            params![id],
            ref_row,
        )?,
        head,
    })
}

pub(super) fn flaw_premises(db: &Db, flaw: NodeId) -> Result<Vec<PremiseRef>> {
    rows(
        db,
        premise_refs!("flaw_premise x", "x.flaw=?1"),
        params![flaw],
        premise_ref,
    )
}

/// The undecided premises candidate `candidate`'s chain rests on.
pub(in crate::store) fn undecided_premises(db: &Db, candidate: NodeId) -> Result<Vec<PremiseRef>> {
    rows(
        db,
        premise_refs!(
            concat!("(", bearing::chain_premises!("?1"), ") x"),
            "p.status='undecided'"
        ),
        params![candidate],
        premise_ref,
    )
}

pub(super) fn flaw(db: &Db, head: Head) -> Result<FlawView> {
    let id = head.id;
    let (component, cwe, status, location) = stored(
        db,
        concat!(
            "SELECT f.component,c.title,f.cwe,f.status,",
            location_columns!(),
            " FROM flaw_state f JOIN node c ON c.id=f.component",
            location_joins!("f.location"),
            " WHERE f.node=?1"
        ),
        params![id],
        |row| {
            Ok((
                Ref {
                    id: NodeId(row.get(0)?),
                    title: row.get(1)?,
                },
                row.get::<Option<u32>>(2)?,
                enum_column(row, 3)?,
                located(row, 4)?,
            ))
        },
    )?;
    Ok(FlawView {
        component,
        cwe,
        status,
        location,
        premises: flaw_premises(db, id)?,
        candidates: rows(
            db,
            "SELECT n.id,n.title,cf.position FROM candidate_flaw cf JOIN node n ON \
             n.id=cf.candidate WHERE cf.flaw=?1 AND n.merged_into IS NULL ORDER BY n.id",
            params![id],
            |row| {
                Ok(Chained {
                    id: CandidateId(row.get(0)?),
                    title: row.get(1)?,
                    position: row.get(2)?,
                })
            },
        )?,
        finding: optional(
            db,
            "SELECT node FROM finding WHERE root=?1",
            params![id],
            |row| Ok(FindingId(row.get(0)?)),
        )?,
        head,
    })
}

pub(super) fn candidate(db: &Db, head: Head) -> Result<CandidateView> {
    let id = head.id;
    let (status, confidence, assessment, verdict) = stored(
        db,
        "SELECT s.state,s.confidence,c.vector,c.severity,a.note,c.verdict,r.note,\
         s.assessed_current,s.reviewed_current FROM candidate c JOIN candidate_state s ON \
         s.candidate=c.node LEFT JOIN event a ON a.id=c.assessed LEFT JOIN event r ON \
         r.id=c.reviewed WHERE c.node=?1",
        params![id],
        |row| {
            let assessment = match row.get::<Option<String>>(2)? {
                Some(vector) => {
                    let parsed: crate::cvss::Cvss40 =
                        (vector.parse()).map_err(|_| corrupt("invalid stored vector"))?;
                    Some(AssessmentView {
                        score: parsed.score(),
                        vector,
                        severity: enum_column(row, 3)?,
                        note: row.get(4)?,
                        current: row.get(7)?,
                    })
                }
                None => None,
            };
            let verdict = (optional_enum_column::<Verdict>(row, 5)?)
                .map(|verdict| -> Result<_> {
                    Ok(VerdictView {
                        verdict,
                        note: row.get(6)?,
                        current: row.get(8)?,
                    })
                })
                .transpose()?;
            Ok((
                enum_column(row, 0)?,
                optional_enum_column(row, 1)?,
                assessment,
                verdict,
            ))
        },
    )?;
    let actions = rows(
        db,
        "SELECT a.path,n.title FROM candidate_action a JOIN node n ON n.id=a.path WHERE \
         a.candidate=?1 ORDER BY a.position",
        params![id],
        ref_row,
    )?;
    let actions = (actions.into_iter())
        .map(|path| {
            Ok(ActionView {
                steps: steps(db, path.id)?,
                sources: sources(db, path.id)?,
                premises: path_premises(db, path.id)?,
                path,
            })
        })
        .collect::<Result<_>>()?;
    let chain = rows(
        db,
        "SELECT f.node,n.title,f.status FROM candidate_flaw cf JOIN flaw_state f ON \
         f.node=cf.flaw JOIN node n ON n.id=f.node WHERE cf.candidate=?1 ORDER BY cf.position",
        params![id],
        |row| {
            Ok(FlawRef {
                id: FlawId(row.get(0)?),
                title: row.get(1)?,
                status: enum_column(row, 2)?,
            })
        },
    )?;
    let chain = (chain.into_iter())
        .map(|flaw| {
            Ok(ChainLink {
                premises: flaw_premises(db, flaw.id.into())?,
                flaw,
            })
        })
        .collect::<Result<_>>()?;
    let mut tests = rows(
        db,
        "SELECT t.id,t.result,e.note,t.environment FROM test t JOIN event e ON e.id=t.event WHERE \
         t.candidate=?1 ORDER BY t.id",
        params![id],
        |row| {
            Ok(TestView {
                id: TestId(row.get(0)?),
                result: enum_column(row, 1)?,
                note: row.get(2)?,
                environment: row.get::<Option<i64>>(3)?.map(EnvironmentId),
                artifacts: Vec::new(),
            })
        },
    )?;
    for test in &mut tests {
        test.artifacts = rows(
            db,
            "SELECT path,role FROM test_artifact WHERE test=?1 ORDER BY path",
            params![test.id],
            |row| {
                Ok(ArtifactView {
                    path: row.get(0)?,
                    role: enum_column(row, 1)?,
                })
            },
        )?;
    }
    Ok(CandidateView {
        actions,
        chain,
        status,
        confidence,
        assessment,
        verdict,
        tests,
        settling_test: stored(
            db,
            concat!("SELECT ", crate::store::bearing::settling_test!("?1")),
            params![id],
            |row| Ok(row.get::<Option<i64>>(0)?.map(TestId)),
        )?,
        // An environment's id and event rise together: ordered by event, each arm reads its
        // index in order.
        environments: environments(
            db,
            "SELECT id,instructions,candidate,event FROM environment WHERE candidate=?1 UNION ALL \
             SELECT id,instructions,candidate,event FROM environment WHERE candidate IS NULL \
             ORDER BY event",
            params![id],
        )?,
        head,
    })
}

pub(super) fn finding(db: &Db, head: Head) -> Result<FindingView> {
    let id = head.id;
    let root = stored(
        db,
        "SELECT n.id,n.title FROM finding f JOIN node n ON n.id=f.root WHERE f.node=?1",
        params![id],
        ref_row,
    )?;
    let scenarios = rows(
        db,
        "SELECT s.candidate,n.title,s.state,s.confidence,s.severity FROM finding_scenario s JOIN \
         node n ON n.id=s.candidate WHERE s.root=?1 ORDER BY s.rank DESC,s.candidate",
        params![root.id],
        |row| {
            Ok(Scenario {
                candidate: candidate_ref(row)?,
                confidence: optional_enum_column(row, 3)?,
                severity: enum_column(row, 4)?,
            })
        },
    )?;
    let current = stored(
        db,
        concat!("SELECT ", queues::current_finding!("?1")),
        params![id],
        |row| Ok(row.get(0)?),
    )?;
    Ok(FindingView {
        root,
        scenarios,
        current,
        head,
    })
}
