//! Tests of candidates and the environments they run in. A test's artifacts are files below
//! the run's artifacts directory, admitted and digested before the test is recorded.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, params};
use skyhook_base::string_newtype;

use crate::store::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}", SERVICE_ADDRESS)]
pub(crate) struct InvalidServiceAddress;

/// What a service address is, as its schema and its refusal state it.
const SERVICE_ADDRESS: &str = "an absolute URL with a host, such as tcp://10.0.0.5:7000, or a file \
                               URL with an absolute path, such as file:///srv/harness/decode";

string_newtype! {
    /// Where an environment serves an interface: an absolute URL with a host, or a file URL
    /// naming a local socket, binary or harness by its absolute path.
    pub(crate) struct ServiceAddress(InvalidServiceAddress) = |address| {
        let valid = match url::Url::parse(address) {
            Ok(url) if url.scheme() == "file" => names_local_path(address, &url),
            _ => address.parse::<http::Uri>().is_ok_and(|uri| {
                uri.scheme().is_some() && uri.host().is_some_and(|host| !host.is_empty())
            }),
        };
        if valid { Ok(()) } else { Err(InvalidServiceAddress) }
    }, schema = schemars::json_schema!({"type": "string", "format": "uri", "description": SERVICE_ADDRESS});
}

/// Whether file URL `url`, parsed from `address`, names an absolute local path as written:
/// parsing drops or escapes whitespace and control characters and keeps malformed escapes, so
/// `address` holds none, and each escape decodes to a byte a path may hold.
fn names_local_path(address: &str, url: &url::Url) -> bool {
    let escapes = address.split('%').skip(1).all(|escaped| {
        (escaped.get(..2))
            .is_some_and(|hex| hex != "00" && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    });
    escapes
        && !address.chars().any(|c| c.is_control() || c.is_whitespace())
        && url.path() != "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.to_file_path().is_ok()
}

// A test's record: its result, the actor's account of it, the environment it ran in and its
// artifacts.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewTest {
    pub(crate) candidate: CandidateId,
    pub(crate) result: TestResult,
    pub(crate) note: Text,
    pub(crate) environment: Option<EnvironmentId>,
    /// Files below the run's artifacts directory, each tagged with its role
    #[serde(default)]
    #[schemars(length(max = MAX_ARTIFACTS))]
    pub(crate) artifacts: Vec<TestArtifact>,
}

// A running instance: its instructions, for one candidate or for any, and the interfaces it
// serves.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewEnvironment {
    pub(crate) instructions: Text,
    pub(crate) candidate: Option<CandidateId>,
    #[serde(default)]
    #[schemars(with = "Listed<Served>")]
    pub(crate) serves: Serves,
}

/// The interfaces an environment serves, each at one address: a request naming one interface
/// twice is no such set.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(try_from = "Listed<Served>")]
pub(crate) struct Serves(std::collections::BTreeMap<InterfaceName, ServiceAddress>);

impl TryFrom<Listed<Served>> for Serves {
    type Error = Error;
    fn try_from(list: Listed<Served>) -> Result<Self> {
        let mut serves = std::collections::BTreeMap::new();
        for Served { interface, address } in list {
            if serves.contains_key(&interface) {
                return Err(Error::ServedTwice(interface));
            }
            serves.insert(interface, address);
        }
        Ok(Self(serves))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub(crate) struct Served {
    pub(crate) interface: InterfaceName,
    pub(crate) address: ServiceAddress,
}

/// Environment `id` is recorded.
pub(in crate::store) fn known_environment(db: &Db, id: EnvironmentId) -> Result<()> {
    match exists(db, "SELECT 1 FROM environment WHERE id=?1", params![id])? {
        true => Ok(()),
        false => Err(Error::NotFound {
            record: Record::Environment,
            id: Some(id.0),
        }),
    }
}

impl Store {
    /// Record a test of a candidate, of what it stood on when the test's task began, or when it
    /// is recorded outside a task. A pending test waits on the user or the environment and
    /// settles nothing.
    pub(crate) fn record_test(&self, actor: &AgentId, test: &NewTest) -> Result<TestId> {
        let mut digested = Vec::with_capacity(test.artifacts.len());
        for mut open in admit(&self.artifact_root(), &test.artifacts)? {
            digested.push((open.digest()?, open.artifact));
        }
        self.write(|db| {
            let candidate = live(db, test.candidate)?;
            if let Some(environment) = test.environment {
                known_environment(db, environment)?;
            }
            audit(db, actor, EventKind::Tested, Some(&test.note), |event| {
                let id = TestId(db.insert(
                    "INSERT INTO test(candidate,environment,result,event) VALUES (?1,?2,?3,?4)",
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

    /// Record an environment and the interfaces it serves. One recorded within a test's work, by its agent or one beneath
    /// it, is for that test's candidate unless it names another, and never offers that test
    /// again.
    pub(crate) fn environment(
        &self,
        actor: &AgentId,
        input: &NewEnvironment,
    ) -> Result<EnvironmentId> {
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
                for (interface, address) in &input.serves.0 {
                    known_interface(db, interface)?;
                    db.execute(
                        "INSERT INTO environment_interface(environment,interface,address) VALUES \
                         (?1,?2,?3)",
                        params![id, interface.as_str(), address.as_str()],
                    )?;
                }
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
    use std::assert_matches;

    use skyhook::job::JobState;

    use super::*;
    use crate::store::tests::{
        artifact, begun, child_of, fixture, operation, scenario, serve, text, validated,
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
        assert_matches!(
            store.record_test(
                &actor,
                &test(vec![attack.clone(), control.clone(), missing])
            ),
            Err(Error::Inadmissible(Inadmissible::Fault { .. }))
        );
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
        let (prover, _, _) = begun(&store, &Begin::Test(Listed::of([merged])));
        let (preparer, _) = child_of(&store, &prover, JobState::Running);
        let merge = Merge {
            node: merged.into(),
            into: kept.into(),
            note: text("one root cause"),
        };
        store.merge(&actor, &merge).unwrap();
        let environment = |actor, candidate| {
            let input = NewEnvironment {
                instructions: text("start the container"),
                candidate,
                serves: Serves::default(),
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

    /// An environment serves recorded interfaces at absolute addresses, each once, and evidence
    /// names the environment it was observed in.
    #[test]
    fn an_environment_serves_recorded_interfaces_and_evidence_names_it() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "gateway");
        let served = [
            "tcp://10.0.0.5:7000",
            "https://staging.example.test/v1",
            "file:///srv/h",
        ];
        for address in served {
            assert!(address.parse::<ServiceAddress>().is_ok(), "{address}");
        }
        let refused = [
            "staging.example.test",
            "/v1/files",
            "https://",
            "file:///",
            "file:///srv/h\0",
            "file:///srv/%zz",
        ];
        for address in refused {
            assert!(address.parse::<ServiceAddress>().is_err(), "{address}");
        }
        let unknown = serve(&store, &actor, &["admin-api"]);
        assert_matches!(unknown, Err(Error::UnknownInterface(_)));
        let served = Served {
            interface: "gateway-api".parse().unwrap(),
            address: "https://staging.example.test".parse().unwrap(),
        };
        let twice = Serves::try_from(Listed::of([served.clone(), served]));
        assert_matches!(twice, Err(Error::ServedTwice(_)));
        let environment = serve(&store, &actor, &["gateway-api"]).unwrap();
        let listed = store.environments(Page::default()).unwrap();
        assert_eq!(
            listed[0]
                .serves
                .iter()
                .map(|served| served.interface.as_str())
                .collect::<Vec<_>>(),
            ["gateway-api"]
        );
        let seen = NewEvidence {
            location: Some(operation("gateway-api")),
            environment: Some(environment),
            stance: Stance::Supports,
            note: text("the control was refused, the probe accepted"),
        };
        store
            .evidence(&actor, scenario.flaw.into(), &[seen])
            .unwrap();
        let view = store.node(scenario.flaw.into()).unwrap();
        assert_eq!(view.head().evidence[0].environment, Some(environment));
    }
}
