use skyhook::tool::{
    RegistryError, ToolRegistryBuilder,
    policy::{Capability, CapabilitySet},
};

use super::{Action, Placement, holds, options, output};
use crate::{
    capabilities::SecurityCapability,
    store::{Environment, EnvironmentId, EnvironmentInput, Listing, Page},
};

actions! {
    enum Operation {
        add_environment => AddEnvironment(EnvironmentInput) -> EnvironmentId,
        list => List {
            #[serde(default)]
            page: Page,
        } -> Listing<Environment>,
    }
}

impl Action for Operation {
    fn permitted(action: Name, capabilities: &CapabilitySet) -> bool {
        match action {
            Name::add_environment => {
                holds(capabilities, [SecurityCapability::Research.capability()])
            }
            Name::list => holds(capabilities, [Capability::Write]),
        }
    }

    fn placement(&self) -> Placement {
        match self {
            Self::List(_) => Placement::Reader,
            Self::AddEnvironment(_) => Placement::Writer,
        }
    }
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    super::register(
        builder,
        "environment",
        "Record or list environment metadata without executing setup or resolving credentials. Endpoints cannot contain credentials, queries or fragments. Supply only named credential references; never put secrets in instructions or other text. Result: JobView<T>: add_environment → environment id; list → keyset page {items: Environment[], next_after?: integer} (continue with page.after = next_after until it is absent).",
        options::<Operation>(),
        |store, actor, _, operation| match operation {
            Operation::AddEnvironment(environment) => {
                output::<result::AddEnvironment>(store.record_environment(actor, &environment))
            }
            Operation::List(list) => output::<result::List>(store.environments(list.page)),
        },
    )
}
