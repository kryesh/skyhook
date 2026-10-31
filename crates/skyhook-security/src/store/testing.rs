//! Tests of candidates and the environments they run in. A test's artifacts are files below
//! the run's artifacts directory, admitted and digested before the test is recorded.

use skyhook::db::params;

use super::*;

/// A test's record: its result, the actor's account of it, the environment it ran in and its
/// artifacts.
#[derive(Clone, Debug)]
pub struct NewTest {
    pub candidate: CandidateId,
    pub result: TestResult,
    pub note: Text,
    pub environment: Option<EnvironmentId>,
    pub artifacts: Vec<TestArtifact>,
}

/// Instructions for running tests, for one candidate or for any.
#[derive(Clone, Debug)]
pub struct NewEnvironment {
    pub instructions: Text,
    pub candidate: Option<CandidateId>,
}

impl Store {
    /// Record a test of a candidate at the verdict it tested: the latest at what the test's task
    /// rests on, or its current one outside a task. A pending test waits on the user or the
    /// environment and settles nothing.
    pub fn record_test(&self, actor: &AgentId, test: &NewTest) -> Result<TestId> {
        let mut digested = Vec::with_capacity(test.artifacts.len());
        for mut open in admit(&self.artifact_root(), &test.artifacts)? {
            digested.push((open.digest()?, open.artifact));
        }
        self.write(|db| {
            let candidate = live(db, test.candidate)?;
            if let Some(environment) = test.environment {
                let known = "SELECT 1 FROM environment WHERE id=?1";
                if !exists(db, known, params![environment])? {
                    return Err(Error::NotFound {
                        record: Record::Environment,
                        id: Some(environment.0),
                    });
                }
            }
            audit(db, actor, EventKind::Tested, Some(&test.note), |event| {
                let id = TestId(db.insert(
                    concat!(
                        "INSERT INTO test(candidate,environment,result,verdict,event) VALUES \
                         (?1,?2,?3,(SELECT max(en.event) FROM event_node en JOIN event e ON \
                         e.id=en.event WHERE en.node=?1 AND e.kind='verdict' AND en.event<=",
                        crate::store::bearing::stamp!("test", "?4", "?1"),
                        "),?4)"
                    ),
                    params![candidate, test.environment, test.result, event],
                )?);
                for (digest, artifact) in &digested {
                    db.execute(
                        "INSERT INTO test_artifact(test,path,role,digest) VALUES (?1,?2,?3,?4)",
                        params![
                            id,
                            artifact.reference.as_str(),
                            artifact.role,
                            digest.as_slice()
                        ],
                    )?;
                }
                let effect = match test.result {
                    TestResult::Pending => EventEffect::Concerns,
                    _ => EventEffect::Standing,
                };
                Ok((id, Marks::of(candidate, effect)))
            })
        })
    }

    /// Record an environment. One recorded within a test's work, by its agent or one beneath
    /// it, is for that test's candidate unless it names another, and never offers that test
    /// again.
    pub fn environment(&self, actor: &AgentId, input: &NewEnvironment) -> Result<EnvironmentId> {
        self.write(|db| {
            let named = input.candidate.map(|id| live(db, id)).transpose()?;
            audit(db, actor, EventKind::Environment, None, |event| {
                let candidate = match named {
                    Some(named) => Some(named),
                    None => optional(
                        db,
                        "SELECT coalesce(n.merged_into,n.id) FROM event_task et JOIN task t ON \
                         t.id=et.task JOIN task_node tn ON tn.task=t.id JOIN node n ON \
                         n.id=tn.node WHERE et.event=?1 AND t.phase='test' ORDER BY t.id DESC,\
                         tn.node LIMIT 1",
                        params![event],
                        |row| Ok(CandidateId(row.get(0)?)),
                    )?,
                };
                let id = EnvironmentId(db.insert(
                    "INSERT INTO environment(instructions,candidate,event) VALUES (?1,?2,?3)",
                    params![&input.instructions, candidate, event],
                )?);
                let mut marks = Marks::default();
                if let Some(candidate) = candidate {
                    marks.mark(candidate, EventEffect::Concerns);
                }
                Ok((id, marks))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use skyhook::job::JobState;

    use super::{
        super::tests::{artifact, begun, child_of, fixture, scenario, text, validated},
        *,
    };

    /// Each artifact's digest is stored as it read when the test was recorded, and an
    /// artifact that is not there is refused.
    #[test]
    fn a_test_records_its_artifacts_digests() {
        let (_session, store, actor) = fixture();
        let id = validated(&store, &actor, "decoder").candidate;
        let attack = artifact(&store, "attack.txt", ArtifactRole::Attack);
        let control = artifact(&store, "control.txt", ArtifactRole::Control);
        let missing = TestArtifact {
            reference: "gone.txt".parse().unwrap(),
            role: ArtifactRole::Notes,
        };
        let test = |artifacts| NewTest {
            candidate: id,
            result: TestResult::Confirmed,
            note: text("ran"),
            environment: None,
            artifacts,
        };
        assert!(matches!(
            store.record_test(
                &actor,
                &test(vec![attack.clone(), control.clone(), missing])
            ),
            Err(Error::Inadmissible(Inadmissible::Fault { .. }))
        ));
        let recorded = store
            .record_test(&actor, &test(vec![attack, control]))
            .unwrap();
        let digests = store
            .read(|db| {
                rows(
                    db,
                    "SELECT path,digest FROM test_artifact WHERE test=?1 ORDER BY path",
                    params![recorded],
                    |row| Ok((row.get::<String>(0)?, row.get::<Vec<u8>>(1)?)),
                )
            })
            .unwrap();
        use sha2::Digest as _;
        let digest = |contents: &str| sha2::Sha256::digest(contents).to_vec();
        assert_eq!(
            digests,
            [
                ("attack.txt".to_owned(), digest("attack.txt")),
                ("control.txt".to_owned(), digest("control.txt"))
            ]
        );
    }

    /// An environment recorded beneath a test is for the candidate it tests, as that candidate
    /// stands after a merge, unless it names another.
    #[test]
    fn an_environment_beneath_a_test_is_for_its_candidate() {
        let (_session, store, actor) = fixture();
        let (merged, kept) = (
            scenario(&store, &actor, "a").candidate,
            scenario(&store, &actor, "b").candidate,
        );
        let (prover, _, _) = begun(&store, &Begin::Test(vec![merged]));
        let (preparer, _) = child_of(&store, &prover, JobState::Running);
        let merge = Merge::Candidate {
            candidate: merged,
            into: kept,
            note: text("one root cause"),
        };
        store.merge(&actor, &merge).unwrap();
        let environment = |actor, candidate| {
            let input = NewEnvironment {
                instructions: text("start the container"),
                candidate,
            };
            store.environment(actor, &input).unwrap()
        };
        let prepared = |id: EnvironmentId| {
            store
                .read(|db| {
                    stored(
                        db,
                        "SELECT candidate FROM environment WHERE id=?1",
                        params![id],
                        |row| Ok(row.get::<Option<i64>>(0)?),
                    )
                })
                .unwrap()
        };
        assert_eq!(prepared(environment(&preparer, None)), Some(kept.0));
        let other = validated(&store, &actor, "c").candidate;
        assert_eq!(prepared(environment(&preparer, Some(other))), Some(other.0));
        assert_eq!(prepared(environment(&actor, None)), None);
    }
}
