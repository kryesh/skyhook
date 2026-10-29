use skyhook::tool::{
    RegistryError, ToolRegistryBuilder,
    policy::{Capability, CapabilitySet},
};

use super::{Action, Placement, holds, options, output};
use crate::{
    capabilities::SecurityCapability,
    reporting::{self, Generated},
    store::{Finding, ReportInput},
};

actions! {
    enum Operation {
        populate => Populate(Box<ReportInput>) -> Finding,
        /// Render the run's reports/ from the store alone, changing nothing when nothing
        /// changed: a folder reports/<root>/ per root flaw whose latest finding is current
        /// (README.md, repro.md, finding.json and a poc-<id>/ folder per stated test holding its
        /// artifacts by role with poc.md, poc.json and SHA256SUMS), and reports/index.md and
        /// index.json listing them. A superseded, stale or withdrawn root's folder and anything
        /// else there is removed, while the store keeps every finding.
        generate => Generate {} -> Generated,
    }
}

impl Action for Operation {
    /// Populating a finding judges what is reported, so it needs verdict authority;
    /// generating the run's reports is the coordinator's.
    fn permitted(action: Name, capabilities: &CapabilitySet) -> bool {
        match action {
            Name::populate => holds(
                capabilities,
                [SecurityCapability::Verdict.capability(), Capability::Write],
            ),
            Name::generate => holds(capabilities, [SecurityCapability::Coordinate.capability()]),
        }
    }

    /// Both read or write artifact and report files around their store work.
    fn placement(&self) -> Placement {
        Placement::Outside
    }
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    super::register(
        builder,
        "finding_report",
        "Findings: one per root flaw, stating every fact from the records around the prose you write. populate records the root's finding: every scenario its group leads to with a positive verdict, headed by the most severe demonstrated one (otherwise the most severe), at the severity of the most severe. A note, narrative or artifact of a scenario or test the report does not state, or a second note or narrative of one, is refused. A root has a new finding each time the scenarios it reports change (a scenario added or left, re-scored, re-validated or re-tested); populating unchanged scenarios again replaces the latest finding's prose. Result: populate → JobView<Finding> {id, root, key, headline, candidates, severity, confidence (the headline's: demonstrated or confirmed), status (current, stale, superseded with by, withdrawn), …}; generate → {reports: root ids written, unwritten: [{root, error}]}.",
        options::<Operation>(),
        |store, actor, context, operation| {
            let cancelled = || context.is_cancelled();
            match operation {
                Operation::Populate(report) => output::<result::Populate>(reporting::populate(
                    store, actor, *report, &cancelled,
                )),
                Operation::Generate(_) => {
                    output::<result::Generate>(reporting::generate(store, &cancelled))
                }
            }
        },
    )
}
