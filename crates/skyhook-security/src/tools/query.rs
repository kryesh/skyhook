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
    ComponentId, Cursor, EnvironmentView, Event, FlawId, FlawStatus, Keyed, ListQuery, Listing,
    NodeId, NodeSummary, NodeView, Page, PremiseScope, PremiseStatus, SpentWork, Startables,
    Status, Store, Synthesis, Task, Tests,
};

/// What a listing reads: live nodes of one kind, filtered, or the environments or tasks.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ListInput {
    Component {
        #[serde(default)]
        page: Page,
    },
    Path {
        #[serde(default)]
        page: Page,
    },
    Source {
        #[serde(default)]
        page: Page,
    },
    Premise {
        scope: Option<PremiseScope>,
        status: Option<PremiseStatus>,
        #[serde(default)]
        page: Page,
    },
    Flaw {
        status: Option<FlawStatus>,
        component: Option<ComponentId>,
        #[serde(default)]
        page: Page,
    },
    Candidate {
        root: Option<FlawId>,
        state: Option<Status>,
        #[serde(default)]
        page: Page,
    },
    Finding {
        #[serde(default)]
        page: Page,
    },
    Environment {
        #[serde(default)]
        page: Page,
    },
    Task {
        #[serde(default)]
        page: Page,
    },
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

impl ListInput {
    fn read(self, store: &Store) -> crate::store::Result<Listing<Listed>> {
        let nodes = |query: ListQuery, page| Ok(listed(store.list(&query, page)?, Listed::Node));
        match self {
            Self::Component { page } => nodes(ListQuery::Component, page),
            Self::Path { page } => nodes(ListQuery::Path, page),
            Self::Source { page } => nodes(ListQuery::Source, page),
            Self::Premise {
                scope,
                status,
                page,
            } => nodes(ListQuery::Premise { scope, status }, page),
            Self::Flaw {
                status,
                component,
                page,
            } => nodes(ListQuery::Flaw { status, component }, page),
            Self::Candidate { root, state, page } => {
                nodes(ListQuery::Candidate { root, state }, page)
            }
            Self::Finding { page } => nodes(ListQuery::Finding, page),
            Self::Environment { page } => {
                Ok(listed(store.environments(page)?, Listed::Environment))
            }
            Self::Task { page } => Ok(listed(store.task_page(page)?, Listed::Task)),
        }
    }
}

actions! {
    enum Query {
        /// The work that can start next; `running` = subjects launched but not yet begun;
        /// `spent` = entries this run launched twice at one basis, exhausted while their phase
        /// owes them at that basis.
        startable => Startable {
            #[serde(default)]
            running: Vec<NodeId>,
            #[serde(default)]
            spent: Vec<SpentWork>,
            #[serde(default)]
            tests: Tests,
            after: Option<Cursor>,
            limit: Bounded<1, 1000>,
        } -> Startables,
        /// What each phase owes, what runs, what two tasks tried without settling (with the
        /// last child's reply), counting `spent` as `startable` does, the deployment premises
        /// the user is asked, and the components no path reaches.
        synthesis => Synthesis {
            #[serde(default)]
            spent: Vec<SpentWork>,
        } -> Synthesis,
        /// A node, merged or not, with its neighbourhood.
        node => Node { id: NodeId } -> NodeView,
        list => List(ListInput) -> Listing<Listed>,
        /// Every event, or the events naming one node, with the nodes each bears on.
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
                store.startable(&a.running, &a.spent, &a.tests, a.after, a.limit),
            ),
            Self::Synthesis(a) => output::<result::Synthesis>(store.synthesis(&a.spent)),
            Self::Node(a) => output::<result::Node>(store.node(a.id)),
            Self::List(list) => output::<result::List>(list.read(store)),
            Self::History(a) => output::<result::History>(store.history(a.node, a.page)),
        }
    }
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    super::register(
        builder,
        "research_query",
        "Read the run's research graph and the work it owes. Every list is a keyset page {items: T[], next_after?: integer}: continue with page.after = next_after until it is absent, even when a page is empty. Result: JobView<T>: startable → {items: [{phase, node, title, basis, about?, pending?, bundle?}], next?, held}; synthesis → {owed, running, exhausted: [{phase, node, title, reply?}], asked: [{premise, title}], unreached: component ids}; node → the node of its kind with its neighbours, evidence and, for a candidate, its chain, assessment, verdict, status, confidence and tests; list → page of {id, title, status?} (an environment or task listing: its records); history → page of {id, kind, agent, at, note?, nodes: [{node, effect}]}.",
        options::<Query>(),
        |store, actor, _, query| query.execute(store, actor),
    )
}
