use schemars::JsonSchema;
use serde::Deserialize;
use skyhook::tool::ToolRegistryBuilder;
use skyhook::tool::catalog::RegistryError;
use skyhook::tool::policy::Capability;

use super::{Documented, Placement, Tool};
use crate::{
    capabilities::SecurityCapability::{Coordinate, Research, Scheduled, Test, Verdict},
    cvss::Cvss40,
    reporting,
    store::{
        Begin, CandidateId, Edge, FlawId, Listed, Merge, NewEnvironment, NewEvidence, NewTest,
        NodeId, Phase, StatusChange, Text, Upsert,
    },
};

/// [`Begin`] as one object: its phase beside the subjects the phase takes.
#[derive(Deserialize)]
#[serde(transparent)]
struct BeginArgs(Begin);

impl JsonSchema for BeginArgs {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Begin".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "phase": generator.subschema_for::<Phase>(),
                "subjects": generator.subschema_for::<Listed<NodeId, 1>>(),
            },
            "required": ["phase", "subjects"],
            "additionalProperties": false,
        })
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpsertArgs {
    record: Upsert,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LinkArgs {
    /// Edges to add; one already there is restated
    #[serde(default)]
    add: Listed<Edge>,
    /// Edges to remove, before any are added
    #[serde(default)]
    remove: Listed<Edge>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EvidenceArgs {
    node: NodeId,
    items: Listed<NewEvidence>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetStatusArgs {
    record: StatusChange,
    note: Text,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AssessArgs {
    candidate: CandidateId,
    /// `CVSS:4.0/AV:_/AC:_/AT:_/PR:_/UI:_/VC:_/VI:_/VA:_/SC:_/SI:_/SA:_`, every base metric once, in that order
    vector: Cvss40,
    note: Text,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct VerdictArgs {
    candidate: CandidateId,
    verdict: crate::store::Verdict,
    note: Text,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReportArgs {
    root: FlawId,
    title: Text,
    body: Text,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GenerateArgs {}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    let research = [Research.capability()];
    let write = |name, description| Tool {
        name,
        description,
        requires: &research,
        placement: Placement::Writer,
        documented: Documented::Generated,
    };
    Tool {
        name: "research_begin",
        description: "Begin the scheduled work your brief assigns, before anything else. Afterwards, job events list changes other agents make to what it rests on; re-read those records before you decide on them\n\nResult: `JobView<{task, scope: the run's scope, {components: [{id, title}], interfaces: [{interface, component}], excluded: [{premise, title}] (failing deployment premises), assumed: [{premise, title, status}] (the other deployment premises)}, changes?: per subject worked before in the phase, what changed since, proposals?: per subject, each result recorded outside this phase for it to decide, premises?: per state checked, the decided premises of the flaws leaving it wrong}>`",
        requires: &[Scheduled.capability()],
        placement: Placement::Writer,
        documented: Documented::Described,
    }
    .register(builder, |store, actor, _, BeginArgs(work)| store.begin(actor, &work))?;
    write(
        "research_upsert",
        "Create a node, `{record: {kind: \"path\", title, body}}`, or edit node `id` of its `kind`, the fields given: `{record: {kind: \"path\", id, title}}`. The result is the node's id",
    )
    .register(builder, |store, actor, _, args: UpsertArgs| store.upsert(actor, &args.record))?;
    write("research_link", "Remove and add edges")
        .register(builder, |store, actor, _, args: LinkArgs| {
            store.relink(actor, &args.add, &args.remove)
        })?;
    write(
        "research_evidence",
        "Record observations about one node. The result is their evidence ids",
    )
    .register(builder, |store, actor, _, args: EvidenceArgs| {
        store.evidence(actor, args.node, &args.items)
    })?;
    write(
        "research_merge",
        "Fold duplicate `node`, of any kind but a finding, into `into`, a node of its kind: its edges move to `into`",
    )
    .register(builder, |store, actor, _, merge: Merge| store.merge(actor, &merge))?;
    write(
        "research_set_status",
        "Decide a premise, judge a flaw, mark a path swept or unswept, mark a state `checked` across calls, or retract a state, path or component recorded in error: it then stands in no work or count until marked `recorded` (a path `unswept`) again. `{record: {kind: \"path\", id, status: \"swept\"}, note}`. A `checked` state names in `rests_on` the premises its clearing relies on: `{record: {kind: \"state\", id, status: \"checked\", rests_on: [premise ids]}, note}`; each owes a settle while undecided, and one failing or contradicted owes the check again",
    )
    .register(builder, |store, actor, _, args: SetStatusArgs| store.set_status(actor, args.record, &args.note))?;
    write(
        "research_assess",
        "Record a candidate's CVSS vector, which the store scores: never compute a score",
    )
    .register(builder, |store, actor, _, args: AssessArgs| {
        store.assess(actor, args.candidate, args.vector, &args.note)
    })?;
    write(
        "research_environment",
        "Record a running instance, for one candidate or any: how to work in it, and the interfaces it serves. The result is its environment id",
    )
    .register(builder, |store, actor, _, environment: NewEnvironment| store.environment(actor, &environment))?;
    // A verdict judges research, so it needs both authorities; a test needs its own, so an
    // agent that tests and records what it observes never decides verdicts.
    Tool {
        requires: &[Research.capability(), Verdict.capability()],
        ..write("research_verdict", "Record a candidate's review verdict")
    }
    .register(builder, |store, actor, _, args: VerdictArgs| {
        store.verdict(actor, args.candidate, args.verdict, &args.note)
    })?;
    // A test admits its artifact files, and generation writes report files, around their
    // store work.
    Tool {
        requires: &[Test.capability()],
        placement: Placement::Outside,
        ..write(
            "research_test",
            "Record a candidate's test. A `pending` result's note says what it waits for. The result is its test id",
        )
    }
    .register(builder, |store, actor, _, test: NewTest| store.record_test(actor, &test))?;
    Tool {
        requires: &[Verdict.capability(), Capability::Write],
        ..write(
            "research_report",
            "Record the root flaw's report: markdown citing node and test ids. The result is its finding id",
        )
    }
    .register(builder, |store, actor, _, args: ReportArgs| store.report(actor, args.root, &args.title, &args.body))?;
    Tool {
        requires: &[Coordinate.capability()],
        placement: Placement::Outside,
        ..write(
            "research_generate",
            "Render the run's reports/ from the store: a folder per root with a current finding, and an index",
        )
    }
    .register(builder, |store, _, context, GenerateArgs {}| reporting::generate(store, &|| context.is_cancelled()))
}
