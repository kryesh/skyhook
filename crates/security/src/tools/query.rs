use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use skyhook::{
    Bounded,
    identity::AgentId,
    tool::{RegistryError, ToolError, ToolRegistryBuilder, policy::CapabilitySet},
};

use super::{Action, Placement, holds, options, output};
use crate::capabilities::SecurityCapability;
use crate::store::{
    ComponentId, Cursor, EnvironmentView, Event, FlawId, FlawStatus, Found, Keyed, ListQuery,
    Listing, Live, NodeId, NodeKind, NodeSummary, NodeView, Page, PremiseScope, PremiseStatus,
    Search, SearchError, SpentWork, Startables, StateScope, Status, Store, Synthesis, Task,
    TaskDetail, TaskId,
};

/// The events a task read shows of its child's, `more` flagging the rest.
const TASK_EVENTS: usize = 100;

/// Records of one kind, filtered: live nodes, environments or tasks
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ListRecord {
    Component {},
    Path {},
    State {
        scope: Option<StateScope>,
    },
    Premise {
        scope: Option<PremiseScope>,
        status: Option<PremiseStatus>,
    },
    Flaw {
        status: Option<FlawStatus>,
        component: Option<ComponentId>,
    },
    Candidate {
        root: Option<FlawId>,
        state: Option<Status>,
    },
    Finding {},
    Environment {},
    Task {},
}

/// A listed record: a node's summary, an environment or a task.
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum Listed {
    Node(NodeSummary),
    Environment(EnvironmentView),
    Task(Task),
}

impl Keyed for Listed {
    fn key(&self) -> i64 {
        match self {
            Self::Node(node) => node.key(),
            Self::Environment(environment) => environment.key(),
            Self::Task(task) => task.key(),
        }
    }
}

fn listed<T>(listing: Listing<T>, wrap: fn(T) -> Listed) -> Listing<Listed> {
    Listing {
        items: listing.items.into_iter().map(wrap).collect(),
        next_after: listing.next_after,
    }
}

impl ListRecord {
    fn read(self, store: &Store, page: Page) -> crate::store::Result<Listing<Listed>> {
        let nodes = |query: ListQuery| Ok(listed(store.list(&query, page)?, Listed::Node));
        match self {
            Self::Component {} => nodes(ListQuery::Component),
            Self::Path {} => nodes(ListQuery::Path),
            Self::State { scope } => nodes(ListQuery::State { scope }),
            Self::Premise { scope, status } => nodes(ListQuery::Premise { scope, status }),
            Self::Flaw { status, component } => nodes(ListQuery::Flaw { status, component }),
            Self::Candidate { root, state } => nodes(ListQuery::Candidate { root, state }),
            Self::Finding {} => nodes(ListQuery::Finding),
            Self::Environment {} => Ok(listed(store.environments(page)?, Listed::Environment)),
            Self::Task {} => Ok(listed(store.task_page(page)?, Listed::Task)),
        }
    }
}

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

actions! {
    enum Query {
        /// The work that can start next; `running` = subjects launched but not yet begun. →
        /// {items: [{phase, node, title, basis, about?, pending?, bundle?}], next?, held}
        startable => Startable {
            #[serde(default)]
            running: Vec<NodeId>,
            #[serde(default)]
            spent: Vec<SpentWork>,
            #[serde(default)]
            live: Live,
            after: Option<Cursor>,
            limit: Bounded<1, 1000>,
        } -> Startables,
        /// What each phase owes, what runs, what two tasks tried without settling (with their
        /// ids and the last child's reply), counting the `spent` entries among
        /// them, the deployment premises the user is asked, the unproven candidates,
        /// the components no path reaches, and the interfaces whose paths wait on an environment.
        /// → {owed, running,
        /// exhausted: [{phase, node, title, tasks?, reply?}], asked: [{premise, title}],
        /// unproven: [{candidate, title, premises}], unreached: component ids, uncovered:
        /// [{interface, paths}], outside: {paths, candidates} counts outside the scope}
        synthesis => Synthesis {
            #[serde(default)]
            spent: Vec<SpentWork>,
        } -> Synthesis,
        /// A node, merged or not. → the node of its kind with its neighbours, evidence, latest
        /// task ids and, for a premise or flaw, any status proposed outside its phase, for a
        /// state, its accesses by path, the flaws leaving it wrong, their decided premises and its
        /// latest check with the premises it rests on, for a candidate, its actions, chain, assessment, verdict and any proposed
        /// outside their phase, status, confidence and tests
        node => Node { id: NodeId } -> NodeView,
        /// A task with its child's brief, reply and first events
        task => Task { id: TaskId } -> TaskDetail,
        /// Records of one kind, filtered inside `record`: `{action: "list", record: {kind:
        /// "flaw", status: "suspected"}, page}`. → page of {id, title, status?}, or of
        /// environment or task records
        list => List {
            record: ListRecord,
            #[serde(default)]
            page: Page,
        } -> Listing<Listed>,
        /// Live nodes by their own text and code locations, a merged node's under the node it was
        /// merged into; needs `pattern`, `file` or `function`. → page of {id, title, status?,
        /// kind, hits: [{in, text}], more?: unlisted hits}
        search => Search(SearchArgs) -> Listing<Found>,
        /// Every event, or the events naming one node. → page of {id, kind, agent, at, note?,
        /// nodes: [{node, effect}]}
        history => History {
            node: Option<NodeId>,
            #[serde(default)]
            page: Page,
        } -> Listing<Event>,
    }
}

impl Action for Query {
    /// What can start next is the coordinator's to schedule.
    fn permitted(action: Name, capabilities: &CapabilitySet) -> bool {
        match action {
            Name::startable => holds(capabilities, [SecurityCapability::Coordinate.capability()]),
            _ => true,
        }
    }

    fn placement(&self) -> Placement {
        Placement::Reader
    }
}

impl Query {
    fn execute(self, store: &Store, _: &AgentId) -> Result<Value, ToolError> {
        match self {
            Self::Startable(a) => output::<result::Startable>(
                store.startable(&a.running, &a.spent, &a.live, a.after, a.limit),
            ),
            Self::Synthesis(a) => output::<result::Synthesis>(store.synthesis(&a.spent)),
            Self::Node(a) => output::<result::Node>(store.node(a.id)),
            Self::Task(a) => output::<result::Task>(store.task(a.id, TASK_EVENTS)),
            Self::List(a) => output::<result::List>(a.record.read(store, a.page)),
            Self::Search(a) => output::<result::Search>(store.search(&a.search, a.page)),
            Self::History(a) => output::<result::History>(store.history(a.node, a.page)),
        }
    }
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    super::register(
        builder,
        "research_query",
        "Read the run's research graph and the work it owes. Every list is a keyset page {items: T[], next_after?: integer}: continue with page.after = next_after until it is absent, even when a page is empty. Result: JobView<T>, T as each action gives after →",
        options::<Query>(),
        |store, actor, _, query| query.execute(store, actor),
    )
}
