use schemars::JsonSchema;
use serde::Deserialize;
use skyhook::tool::ToolRegistryBuilder;
use skyhook::tool::catalog::RegistryError;
use skyhook_base::bounded::Bounded;

use super::{Documented, Placement, Tool};
use crate::capabilities::SecurityCapability;
use crate::store::{
    Cursor, ListQuery, Live, NodeId, NodeKind, Page, Search, SearchError, SpentWork, TaskId,
};

/// The events a task read shows of its child's, `more` flagging the rest.
const TASK_EVENTS: usize = 100;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    /// Regex (`(?i)` ignores case) over each line of a node's title, body, notes, locations, files and interfaces
    pattern: Option<String>,
    /// Glob over its code locations' files and a component's files
    file: Option<String>,
    /// With `file`: a line its code location spans
    #[schemars(range(min = 1))]
    line: Option<u32>,
    /// Regex over its code locations' function
    function: Option<String>,
    /// Default: all
    kinds: Option<Vec<NodeKind>>,
    #[serde(default)]
    page: Page,
}

#[derive(Deserialize)]
#[serde(try_from = "SearchInput")]
struct SearchArgs {
    search: Search,
    page: Page,
}

impl JsonSchema for SearchArgs {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        SearchInput::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        SearchInput::json_schema(generator)
    }
}

impl TryFrom<SearchInput> for SearchArgs {
    type Error = SearchError;
    fn try_from(input: SearchInput) -> Result<Self, SearchError> {
        let search = Search::new(
            input.pattern.as_deref(),
            input.file.as_deref(),
            input.line,
            input.function.as_deref(),
            input.kinds.unwrap_or_else(|| NodeKind::ALL.to_vec()),
        )?;
        Ok(Self {
            search,
            page: input.page,
        })
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartableArgs {
    /// Subjects launched but not yet begun
    #[serde(default)]
    running: Vec<NodeId>,
    #[serde(default)]
    spent: Vec<SpentWork>,
    #[serde(default)]
    live: Live,
    /// The last page's `next`
    after: Option<Cursor>,
    limit: Bounded<1, 1000>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SynthesisArgs {
    #[serde(default)]
    spent: Vec<SpentWork>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NodeArgs {
    id: NodeId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TaskArgs {
    id: TaskId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    record: ListQuery,
    #[serde(default)]
    page: Page,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HistoryArgs {
    /// Only the events naming this node
    node: Option<NodeId>,
    #[serde(default)]
    page: Page,
}

/// A description ending with how its keyset page of `items` continues.
macro_rules! paged {
    ($description:literal, $items:literal) => {
        concat!(
            $description,
            ". Continue with page.after = next_after until it is absent, even when a page is empty\n\nResult: `JobView<{items: ",
            $items,
            ", next_after?: integer}>`"
        )
    };
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    let read = |name, description, documented| Tool {
        name,
        description,
        requires: &[],
        placement: Placement::Reader,
        documented,
    };
    // What can start next is the coordinator's to schedule.
    Tool {
        requires: &[SecurityCapability::Coordinate.capability()],
        ..read(
            "research_startable",
            "The work that can start next. Continue with after = next until next is null",
            Documented::Generated,
        )
    }
    .register(builder, |store, _, _, a: StartableArgs| {
        store.startable(&a.running, &a.spent, &a.live, a.after, a.limit)
    })?;
    read(
        "research_synthesis",
        "What the run owes\n\nResult: `JobView<{owed: per phase, running, exhausted: [{phase, node, title, tasks?, reply?}] (two tasks tried without settling, spent entries included), asked: [{premise, title}] (deployment premises put to the user), unproven: [{candidate, title, premises}], unreached: component ids no path reaches, uncovered: [{interface, paths}] (waiting on an environment), outside: {paths, candidates} counts outside the scope}>`",
        Documented::Described,
    )
    .register(builder, |store, _, _, a: SynthesisArgs| store.synthesis(&a.spent))?;
    read(
        "research_node",
        "Read a node, merged or not: the node of its kind with its neighbours, evidence, latest task ids and, for a premise or flaw, any status proposed outside its phase, for a state, its accesses by path, the flaws leaving it wrong, their decided premises and its latest check with the premises it rests on, for a candidate, its actions, chain, assessment, verdict and any proposed outside their phase, status, confidence and tests",
        Documented::Described,
    )
    .register(builder, |store, _, _, a: NodeArgs| store.node(a.id))?;
    read(
        "research_task",
        "Read a task with its child's brief, reply and first events",
        Documented::Described,
    )
    .register(builder, |store, _, _, a: TaskArgs| {
        store.task(a.id, TASK_EVENTS)
    })?;
    read(
        "research_list",
        paged!(
            "List records of one kind, filtered inside `record`: `{record: {kind: \"flaw\", status: \"suspected\"}, page}`",
            "[{id, title, status?}] or environment or task records"
        ),
        Documented::Described,
    )
    .register(builder, |store, _, _, a: ListArgs| store.list(&a.record, a.page))?;
    read(
        "research_search",
        paged!(
            "Find live nodes by their own text and code locations, a merged node's under the node it was merged into; needs `pattern`, `file` or `function`",
            "[{id, title, status?, kind, hits: [{in, text}], more?: unlisted hits}]"
        ),
        Documented::Described,
    )
    .register(builder, |store, _, _, a: SearchArgs| store.search(&a.search, a.page))?;
    read(
        "research_history",
        paged!(
            "Every event, or the events naming one node",
            "[{id, kind, agent, at, note?, nodes: [{node, effect}]}]"
        ),
        Documented::Described,
    )
    .register(builder, |store, _, _, a: HistoryArgs| {
        store.history(a.node, a.page)
    })
}
