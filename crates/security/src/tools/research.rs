use std::{borrow::Cow, sync::LazyLock};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use skyhook::{
    identity::AgentId,
    tool::{
        AdmissionError, RegistryError, ToolContext, ToolError, ToolRegistryBuilder,
        arguments::deserialize_arguments,
        policy::{Capability, CapabilitySet},
    },
};

use super::{Action, Placement, holds, options, output};
use crate::{
    capabilities::SecurityCapability,
    cvss::Cvss40,
    reporting::{self, Generated},
    store::{
        Assessment, Begun, CandidateId, EditRecord, EnvironmentId, EvidenceId, FindingId, FlawId,
        MAX_ARTIFACTS, MAX_ATTACHMENTS, NewEnvironment, NewEvidence, NewRecord, NewTest, NodeId,
        Served, Serves, StatusRecord, Store, TestArtifact, TestId, TestResult, Text, Upsert,
    },
};

actions! {
    enum Research {
        /// When your brief assigns you scheduled work, begin it before anything else. Its result
        /// names what changed for each subject since its last task in this phase.
        /// Afterwards, job events list changes other agents make to what it rests on; re-read
        /// those records before you decide on them. → {task, scope: the run's scope, {components:
        /// [{id, title}], interfaces: [{interface, component}], excluded: [{premise, title}]
        /// (failing deployment premises), assumed: [{premise, title, status}] (the other
        /// deployment premises)}, changes?: per subject worked before
        /// in the phase, what changed since, proposals?: per subject, each result recorded
        /// outside this phase for it to decide, premises?: per state checked, the decided premises of the
        /// flaws leaving it wrong}
        begin => Begin(crate::store::Begin) -> Begun,
        /// Create a node, its fields inside `record`: `{action: "upsert", record: {kind: "path",
        /// title, body}}`; or edit node `id`, the fields given: `{action: "upsert", record: {id,
        /// title}}`. → node id
        upsert => Upsert { record: UpsertRecord } -> NodeId,
        /// Add edges; an ordered one at `position`, or last
        link => Link {
            #[schemars(length(max = MAX_ATTACHMENTS))]
            links: Vec<crate::store::Link>,
        } -> (),
        /// Remove edges; a path step by its position
        unlink => Unlink {
            #[schemars(length(max = MAX_ATTACHMENTS))]
            links: Vec<crate::store::Unlink>,
        } -> (),
        /// Observations about one node. → evidence ids
        evidence => Evidence {
            node: NodeId,
            #[schemars(length(max = MAX_ATTACHMENTS))]
            items: Vec<NewEvidence>,
        } -> Vec<EvidenceId>,
        merge => Merge(crate::store::Merge) -> (),
        /// Decide a premise, judge a flaw, mark a path swept or unswept, mark a state `checked`
        /// across calls, or retract a state, path or component recorded in error: it then stands
        /// in no work or count until marked `recorded` (a path `unswept`) again.
        /// `{action: "set_status", record: {kind: "path", id, status: "swept"}, note}`. A
        /// `checked` state names in `rests_on` the premises its clearing relies on:
        /// `{record: {kind: "state", id, status: "checked", rests_on: [premise ids]}, note}`; each
        /// owes a settle while undecided, and one failing or contradicted owes the check again
        set_status => SetStatus { record: StatusRecord, note: Text } -> (),
        /// The store scores `vector`: record the vector, never compute a score. → {severity,
        /// score, shortlisted}
        assess => Assess {
            candidate: CandidateId,
            /// `CVSS:4.0/AV:_/AC:_/AT:_/PR:_/UI:_/VC:_/VI:_/VA:_/SC:_/SI:_/SA:_`, every base metric once, in that order
            vector: Cvss40,
            note: Text,
        } -> Assessment,
        verdict => Verdict { candidate: CandidateId, verdict: crate::store::Verdict, note: Text } -> (),
        /// A `pending` result's note says what it waits for. → test id
        test => Test {
            candidate: CandidateId,
            result: TestResult,
            note: Text,
            environment: Option<EnvironmentId>,
            /// Files below the run's artifacts directory, each tagged with its role
            #[serde(default)]
            #[schemars(length(max = MAX_ARTIFACTS))]
            artifacts: Vec<TestArtifact>,
        } -> TestId,
        /// A running instance, for one candidate or any: how to work in it, and the interfaces
        /// it serves. → environment id
        environment => Environment {
            instructions: Text,
            candidate: Option<CandidateId>,
            #[serde(default)]
            #[schemars(with = "Vec<Served>", length(max = MAX_ATTACHMENTS))]
            serves: Serves,
        } -> EnvironmentId,
        /// The root flaw's report: markdown citing node and test ids. → finding id
        report => Report { root: FlawId, title: Text, body: Text } -> FindingId,
        /// Render the run's reports/ from the store: a folder per root with a current
        /// finding, and an index. → {reports: root ids written, unwritten: [{root, error}]}
        generate => Generate {} -> Generated,
    }
    unions: [links, record]
}

impl Action for Research {
    fn permitted(action: Name, capabilities: &CapabilitySet) -> bool {
        let [research, verdict, test, scheduled, coordinate] =
            SecurityCapability::ALL.map(SecurityCapability::capability);
        match action {
            Name::begin => holds(capabilities, [scheduled]),
            // A verdict judges research, so it needs both authorities; a test needs its own,
            // so an agent that tests and records what it observes never decides verdicts.
            Name::verdict => holds(capabilities, [research, verdict]),
            Name::test => holds(capabilities, [test]),
            Name::report => holds(capabilities, [verdict, Capability::Write]),
            Name::generate => holds(capabilities, [coordinate]),
            _ => holds(capabilities, [research]),
        }
    }

    /// A test admits its artifact files, and generation writes report files, around their
    /// store work.
    fn placement(&self) -> Placement {
        match self {
            Self::Test(_) | Self::Generate(_) => Placement::Outside,
            _ => Placement::Writer,
        }
    }
}

impl Research {
    fn execute(
        self,
        store: &Store,
        actor: &AgentId,
        context: &ToolContext,
    ) -> Result<Value, ToolError> {
        match self {
            Self::Begin(work) => output::<result::Begin>(store.begin(actor, &work)),
            Self::Upsert(a) => {
                output::<result::Upsert>(store.upsert(actor, &a.record.read(store)?))
            }
            Self::Link(a) => output::<result::Link>(store.link(actor, &a.links)),
            Self::Unlink(a) => output::<result::Unlink>(store.unlink(actor, &a.links)),
            Self::Evidence(a) => {
                output::<result::Evidence>(store.evidence(actor, a.node, &a.items))
            }
            Self::Merge(merge) => output::<result::Merge>(store.merge(actor, &merge)),
            Self::SetStatus(a) => {
                output::<result::SetStatus>(store.set_status(actor, a.record.into(), &a.note))
            }
            Self::Assess(a) => {
                output::<result::Assess>(store.assess(actor, a.candidate, a.vector, &a.note))
            }
            Self::Verdict(a) => {
                output::<result::Verdict>(store.verdict(actor, a.candidate, a.verdict, &a.note))
            }
            Self::Test(a) => {
                let test = NewTest {
                    candidate: a.candidate,
                    result: a.result,
                    note: a.note,
                    environment: a.environment,
                    artifacts: a.artifacts,
                };
                output::<result::Test>(store.record_test(actor, &test))
            }
            Self::Environment(a) => {
                let environment = NewEnvironment {
                    instructions: a.instructions,
                    candidate: a.candidate,
                    serves: a.serves,
                };
                output::<result::Environment>(store.environment(actor, &environment))
            }
            Self::Report(a) => {
                output::<result::Report>(store.report(actor, a.root, &a.title, &a.body))
            }
            Self::Generate(_) => {
                output::<result::Generate>(reporting::generate(store, &|| context.is_cancelled()))
            }
        }
    }
}

/// An upsert's record, read as a new node's without an `id`, else as an edit's, whose kind,
/// when it leaves it out, is node `id`'s.
#[derive(Deserialize)]
#[serde(transparent)]
struct UpsertRecord(Map<String, Value>);

/// Arguments holding a `record`, so a refusal is located inside it.
#[derive(Deserialize, JsonSchema)]
struct Recorded<T> {
    record: T,
}

impl UpsertRecord {
    fn read(mut self, store: &Store) -> Result<Upsert, ToolError> {
        #[derive(Deserialize)]
        struct Edit {
            id: NodeId,
        }
        let upsert = match self.0.contains_key("id") {
            false => Upsert::try_from(self.parse::<NewRecord>()?),
            true => {
                if !self.0.contains_key("kind") {
                    let kind = store.kind(self.parse::<Edit>()?.id);
                    let kind = kind.map_err(ToolError::failed)?;
                    self.0.insert("kind".to_owned(), kind.as_str().into());
                }
                Upsert::try_from(self.parse::<EditRecord>()?)
            }
        };
        upsert.map_err(ToolError::failed)
    }

    fn parse<T: DeserializeOwned>(&self) -> Result<T, AdmissionError> {
        static SCHEMA: LazyLock<Value> =
            LazyLock::new(skyhook::tool::input_schema_for::<Recorded<UpsertRecord>>);
        let arguments = Map::from_iter([("record".to_owned(), Value::Object(self.0.clone()))]);
        deserialize_arguments::<Recorded<T>>(&SCHEMA, &arguments).map(|read| read.record)
    }
}

impl JsonSchema for UpsertRecord {
    fn schema_name() -> Cow<'static, str> {
        "UpsertRecord".into()
    }

    /// A new node, an edit of one of a kind, or an edit leaving its kind out.
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        schemars::json_schema!({"anyOf": [
            generator.subschema_for::<NewRecord>(),
            generator.subschema_for::<EditRecord>(),
            {
                "description": "An edit of node `id` leaving out its `kind`: the fields its kind takes",
                "type": "object",
                "properties": {"id": generator.subschema_for::<NodeId>()},
                "required": ["id"],
            },
        ]})
    }
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    super::register(
        builder,
        "research",
        "Record the run's research graph. Result: JobView<T>, T as each action gives after →, else null",
        options::<Research>(),
        |store, actor, context, research| research.execute(store, actor, context),
    )
}
