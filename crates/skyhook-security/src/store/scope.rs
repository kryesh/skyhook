//! The research boundary: scopes with the deployment facts they admit or exclude,
//! principals, sources and the code locations evidence names.

use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, params};

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Scope {
    pub id: ScopeId,
    pub description: String,
    pub deployment: Vec<Deployment>,
    /// Conditions asked of the user under this scope that nothing decided since; the
    /// latest scope's wait for the user's answer, recorded as a new scope.
    pub asked: Vec<AskedCondition>,
}

/// A condition a candidate relies on that the scope's facts do not address, with the question
/// put to the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AskedCondition {
    pub premise: PremiseId,
    pub candidate: CandidateId,
    pub statement: String,
    pub question: String,
}

/// The run's policy: the latest scope, if one is recorded.
pub(super) fn policy(db: &Db) -> Result<Option<ScopeId>> {
    Ok(
        optional(db, "SELECT max(id) FROM scope", Vec::new(), |row| {
            Ok(row.get::<Option<i64>>(0)?)
        })?
        .flatten()
        .map(ScopeId),
    )
}

/// A fact about the deployment in scope that conditions resolve against: a build flag,
/// platform, host behaviour or input bound it admits, or one it excludes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewDeployment {
    pub kind: DeploymentKind,
    pub stance: DeploymentStance,
    pub statement: Text,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Deployment {
    pub id: DeploymentId,
    pub kind: DeploymentKind,
    pub stance: DeploymentStance,
    pub statement: String,
}

/// Refuse a citation of `deployment` unless the current scope states it with `stance`.
pub(super) fn cite(db: &Db, deployment: DeploymentId, stance: DeploymentStance) -> Result<()> {
    let (stated, current) = one(
        db,
        "SELECT stance,scope=(SELECT max(id) FROM scope) FROM deployment WHERE id=?1",
        deployment,
        |row| {
            Ok((
                enum_column::<DeploymentStance>(row, 0)?,
                row.get::<i64>(1)? != 0,
            ))
        },
    )?;
    if !current {
        Err(Error::DeploymentSuperseded { deployment })
    } else if stated != stance {
        Err(Error::DeploymentStance {
            deployment,
            stance: stated,
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPrincipal {
    pub name: Text,
    pub trust: Trust,
    pub description: Text,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Principal {
    pub id: PrincipalId,
    pub name: String,
    pub trust: Trust,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSource {
    pub kind: SourceKind,
    pub trust: Trust,
    /// Where the input is read, received or parsed.
    pub entry: Location,
    pub description: Text,
    /// Principals that can drive this input; empty when not yet established.
    #[serde(default)]
    pub principals: Vec<PrincipalId>,
    /// Where its input is handed into implementation; none while unresolved, such as
    /// dynamic registration or generated dispatch.
    #[serde(default)]
    pub handoff: Option<Handoff>,
}

/// The first call into implementation code beyond parsing and dispatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    /// The call site.
    pub at: Location,
    /// The function called there.
    pub calls: Text,
}

/// A recorded handoff, with the component owning its file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceHandoff {
    #[serde(flatten)]
    pub handoff: Handoff,
    pub component: Option<ComponentId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Source {
    pub id: SourceId,
    pub kind: SourceKind,
    pub trust: Trust,
    pub entry: Location,
    pub description: String,
    pub principals: Vec<PrincipalId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<SourceHandoff>,
}

/// The location's row, created on first use.
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
        "INSERT INTO location(file,start_line,end_line,function) VALUES (?1,?2,?3,?4) ON CONFLICT DO NOTHING",
        key(),
    )?;
    stored(
        db,
        "SELECT id FROM location WHERE file=?1 AND start_line=?2 AND end_line=?3 AND function IS ?4",
        key(),
        |row| Ok(LocationId(row.get(0)?)),
    )
}

/// A location joined as `$alias`, decoded by [`location_column`].
macro_rules! location_columns {
    ($alias:literal) => {
        concat!(
            $alias,
            ".file,",
            $alias,
            ".start_line,",
            $alias,
            ".end_line,",
            $alias,
            ".function"
        )
    };
}
pub(super) use location_columns;

pub(super) fn location_column(row: &Row, offset: i32) -> Result<Location> {
    Location::new(
        row.get::<String>(offset)?.parse()?,
        row.get::<i64>(offset + 1)? as u32,
        row.get::<i64>(offset + 2)? as u32,
        (row.get::<Option<String>>(offset + 3)?)
            .map(|function| function.parse())
            .transpose()?,
    )
}

fn source_row(row: &Row) -> Result<Source> {
    Ok(Source {
        id: SourceId(row.get(0)?),
        kind: enum_column(row, 1)?,
        trust: enum_column(row, 2)?,
        description: row.get(3)?,
        entry: location_column(row, 4)?,
        principals: Vec::new(),
        handoff: match row.get::<Option<String>>(8)? {
            Some(calls) => Some(SourceHandoff {
                handoff: Handoff {
                    at: location_column(row, 9)?,
                    calls: calls.parse()?,
                },
                component: row.get::<Option<i64>>(13)?.map(ComponentId),
            }),
            None => None,
        },
    })
}

fn principal_row(row: &Row) -> Result<Principal> {
    Ok(Principal {
        id: PrincipalId(row.get(0)?),
        name: row.get(1)?,
        trust: enum_column(row, 2)?,
        description: row.get(3)?,
    })
}

pub(super) fn source(db: &Db, id: SourceId) -> Result<Source> {
    let mut source = one(
        db,
        concat!(
            "SELECT s.id,s.kind,s.trust,s.description,",
            location_columns!("l"),
            ",h.calls,",
            location_columns!("hl"),
            ",cf.component FROM source s JOIN location l ON l.id=s.entry LEFT JOIN source_handoff h \
             ON h.source=s.id LEFT JOIN location hl ON hl.id=h.location LEFT JOIN component_file \
             cf ON cf.file=hl.file WHERE s.id=?1"
        ),
        id,
        source_row,
    )?;
    source.principals = rows(
        db,
        "SELECT principal FROM source_principal WHERE source=?1 ORDER BY principal",
        params![id.0],
        |row| Ok(PrincipalId(row.get(0)?)),
    )?;
    Ok(source)
}

pub(super) fn principal(db: &Db, id: PrincipalId) -> Result<Principal> {
    one(
        db,
        "SELECT id,name,trust,description FROM principal WHERE id=?1",
        id,
        principal_row,
    )
}

/// Refuse an id that names no record.
pub(super) fn known<I: RecordId>(db: &Db, id: I) -> Result<()> {
    one(db, I::KNOWN, id, |_| Ok(()))
}

impl Store {
    /// A new scope, with the deployment facts it admits or excludes, becomes the run's
    /// policy: policy exclusions recorded under an earlier one and candidates whose conditions
    /// were asked under one go back to review, and a condition holding by an earlier scope's
    /// deployment fact reopens.
    pub fn add_scope(
        &self,
        actor: &AgentId,
        description: &Text,
        deployment: &[NewDeployment],
    ) -> Result<ScopeId> {
        if deployment.len() > MAX_ATTACHMENTS {
            return Err(Error::Limit(Bound::Attachments));
        }
        self.write(|db| {
            let id = ScopeId(db.insert(
                "INSERT INTO scope(description) VALUES (?1)",
                params![description],
            )?);
            for fact in deployment {
                db.execute(
                    "INSERT INTO deployment(scope,kind,stance,statement) VALUES (?1,?2,?3,?4)",
                    params![id.0, fact.kind, fact.stance, &fact.statement],
                )?;
            }
            audit(
                db,
                actor,
                EventKind::Recorded,
                Detail::Record(EventRecord::Scope(id)),
                &[],
            )?;
            for premise in rows(
                db,
                "SELECT x.premise FROM premise_decision x JOIN deployment d ON \
                 d.id=x.deployment WHERE x.current AND d.scope!=?1 ORDER BY x.premise",
                params![id.0],
                |row| Ok(PremiseId(row.get(0)?)),
            )? {
                premises::touch(db, actor, premise)?;
            }
            Ok(id)
        })
    }

    pub fn list_scopes(&self, page: Page) -> Result<Listing<Scope>> {
        self.read(|db| {
            let scopes = rows(
                db,
                "SELECT id,description FROM scope WHERE id>?1 ORDER BY id LIMIT ?2",
                params![page.after, page.limit()],
                |row| {
                    Ok(Scope {
                        id: ScopeId(row.get(0)?),
                        description: row.get(1)?,
                        deployment: Vec::new(),
                        asked: Vec::new(),
                    })
                },
            )?;
            let scopes = (scopes.into_iter())
                .map(|mut scope| {
                    scope.deployment = rows(
                        db,
                        "SELECT id,kind,stance,statement FROM deployment WHERE scope=?1 ORDER BY id",
                        params![scope.id.0],
                        |row| {
                            Ok(Deployment {
                                id: DeploymentId(row.get(0)?),
                                kind: enum_column(row, 1)?,
                                stance: enum_column(row, 2)?,
                                statement: row.get(3)?,
                            })
                        },
                    )?;
                    scope.asked = rows(
                        db,
                        concat!(
                            "SELECT p.id,cc.candidate,p.statement,x.note FROM ",
                            premises::asked!("?1"),
                            " JOIN candidate_condition cc ON cc.premise=p.id WHERE ",
                            paths::live_candidate!("cc.candidate"),
                            " ORDER BY p.id,cc.candidate"
                        ),
                        params![scope.id.0],
                        |row| {
                            Ok(AskedCondition {
                                premise: PremiseId(row.get(0)?),
                                candidate: CandidateId(row.get(1)?),
                                statement: row.get(2)?,
                                question: row.get(3)?,
                            })
                        },
                    )?;
                    Ok(scope)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Listing::keyset(scopes, page))
        })
    }

    /// Principals are keyed by name; re-adding one replaces its trust and description.
    /// A changed trust is a material change to every candidate on a path whose sources
    /// the principal drives.
    pub fn add_principal(&self, actor: &AgentId, input: &NewPrincipal) -> Result<PrincipalId> {
        self.write(|db| {
            let recorded = optional(
                db,
                "SELECT id,trust FROM principal WHERE name=?1",
                params![&input.name],
                |row| Ok((PrincipalId(row.get(0)?), enum_column::<Trust>(row, 1)?)),
            )?;
            db.execute(
                "INSERT INTO principal(name,trust,description) VALUES (?1,?2,?3) ON CONFLICT(name) DO UPDATE SET trust=excluded.trust,description=excluded.description",
                params![&input.name, input.trust, &input.description],
            )?;
            let id = stored(
                db,
                "SELECT id FROM principal WHERE name=?1",
                params![&input.name],
                |row| Ok(PrincipalId(row.get(0)?)),
            )?;
            if recorded.is_some_and(|(_, trust)| trust != input.trust) {
                let affected = rows(
                    db,
                    "SELECT DISTINCT cp.candidate FROM source_principal sp JOIN path_source ps ON ps.source=sp.source JOIN candidate_path cp ON cp.path=ps.path WHERE sp.principal=?1",
                    params![id.0],
                    |row| Ok(CandidateId(row.get(0)?)),
                )?;
                flaws::touch_candidates(db, &affected)?;
            }
            audit(db, actor, EventKind::Recorded, Detail::Record(EventRecord::Principal(id)), &[])?;
            Ok(id)
        })
    }

    pub fn list_principals(&self, page: Page) -> Result<Listing<Principal>> {
        self.read(|db| {
            let principals = rows(
                db,
                "SELECT id,name,trust,description FROM principal WHERE id>?1 ORDER BY id LIMIT ?2",
                params![page.after, page.limit()],
                principal_row,
            )?;
            Ok(Listing::keyset(principals, page))
        })
    }

    pub fn add_source(&self, actor: &AgentId, input: &NewSource) -> Result<SourceId> {
        self.write(|db| {
            for principal in &input.principals {
                known(db, *principal)?;
            }
            let entry = location_id(db, &input.entry)?;
            let id = SourceId(db.insert(
                "INSERT INTO source(kind,trust,entry,description) VALUES (?1,?2,?3,?4)",
                params![input.kind, input.trust, entry.0, &input.description],
            )?);
            for principal in &input.principals {
                db.execute(
                    "INSERT INTO source_principal(source,principal) VALUES (?1,?2) ON CONFLICT DO NOTHING",
                    params![id.0, principal.0],
                )?;
            }
            if let Some(handoff) = &input.handoff {
                let at = location_id(db, &handoff.at)?;
                db.execute(
                    "INSERT INTO source_handoff(source,location,calls) VALUES (?1,?2,?3)",
                    params![id.0, at.0, &handoff.calls],
                )?;
            }
            audit(db, actor, EventKind::Recorded, Detail::Record(EventRecord::Source(id)), &[])?;
            Ok(id)
        })
    }

    pub fn source(&self, id: SourceId) -> Result<Source> {
        self.read(|db| source(db, id))
    }

    pub fn list_sources(&self, page: Page) -> Result<Listing<Source>> {
        self.read(|db| {
            let ids = rows(
                db,
                "SELECT id FROM source WHERE id>?1 ORDER BY id LIMIT ?2",
                params![page.after, page.limit()],
                |row: &Row| Ok(SourceId(row.get(0)?)),
            )?;
            let sources = ids
                .into_iter()
                .map(|id| source(db, id))
                .collect::<Result<Vec<_>>>()?;
            Ok(Listing::keyset(sources, page))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{fixture, new_premise, scenario, shortlisted, status, text, verdict},
        *,
    };

    /// A condition holds by what the scope admits and a policy exclusion cites what it
    /// excludes, each only while that scope is current: a new scope reopens the condition and
    /// returns the exclusion to review.
    #[test]
    fn deployment_facts_settle_conditions_while_their_scope_stands() {
        let (_directory, store, actor) = fixture();
        let fact = |kind, stance, statement| NewDeployment {
            kind,
            stance,
            statement: text(statement),
        };
        let first = store
            .add_scope(
                &actor,
                &text("embedding hosts"),
                &[
                    fact(
                        DeploymentKind::BuildFlag,
                        DeploymentStance::Admitted,
                        "experimental builds",
                    ),
                    fact(
                        DeploymentKind::Platform,
                        DeploymentStance::Excluded,
                        "32-bit builds",
                    ),
                ],
            )
            .unwrap();
        let scope = |id| {
            (store
                .list_scopes(Page::default())
                .unwrap()
                .items
                .into_iter())
            .find(|scope| scope.id == id)
            .unwrap()
        };
        let [admitted, excluded] = [0, 1].map(|index| scope(first).deployment[index].id);
        let id = shortlisted(&store, &actor);
        let premise = store
            .add_premise(
                &actor,
                scenario(id),
                &new_premise(InvariantKind::State, "the experimental flag is on"),
            )
            .unwrap();
        let holds = |deployment| {
            store.resolve_premise(
                &actor,
                premise,
                Resolution::Holds {
                    deployment: Some(deployment),
                },
                &text("the scope admits it"),
            )
        };
        assert!(matches!(
            holds(excluded),
            Err(Error::DeploymentStance { .. })
        ));
        holds(admitted).unwrap();
        verdict(&store, &actor, id, Verdict::Validated);
        assert_eq!(status(&store, id), Status::Validated);
        let excluding = Verdict::PolicyExcluded {
            deployment: Some(excluded),
        };
        let out_of_scope = shortlisted(&store, &actor);
        verdict(&store, &actor, out_of_scope, excluding);
        assert_eq!(status(&store, out_of_scope), Status::Rejected);

        let second = store
            .add_scope(&actor, &text("hosts revised"), &[])
            .unwrap();
        assert!(store.premise(premise).unwrap().standing().is_none());
        assert_eq!(status(&store, id), Status::Shortlisted);
        assert_eq!(status(&store, out_of_scope), Status::Shortlisted);
        assert!(matches!(
            holds(admitted),
            Err(Error::DeploymentSuperseded { .. })
        ));
        assert!(scope(second).deployment.is_empty());
        assert!(matches!(
            store.record_validation(&actor, id, excluding, &text("32-bit only")),
            Err(Error::DeploymentSuperseded { .. })
        ));
    }
}
