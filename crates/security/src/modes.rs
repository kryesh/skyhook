//! Embedded research modes. Named security.yaml modes replace these whole.

use indexmap::IndexMap;
use skyhook::config::mode::{Instructions, Mode, ModeName};
use skyhook::tool::policy::{Capability, CapabilitySet};

use crate::capabilities::SecurityCapability;

/// The only root mode: every session's root orchestrates.
pub(crate) fn root() -> ModeName {
    name("orchestrate")
}

fn name(value: &str) -> ModeName {
    value.parse().expect("embedded mode name is nonblank")
}

/// What every research mode holds. Execution and network access are granted only to the
/// modes whose work needs them, and to the parents that launch those modes. Delegation is
/// bounded by each launch's depth, not by mode. A mode without a hint is never offered to
/// a child: only the root orchestrates.
const BASE: [Capability; 3] = [Capability::Read, Capability::Mcp, Capability::Agents];

/// The mode `name`, instructed by its template.
fn mode(
    name: &str,
    extra: &[Capability],
    security: &[SecurityCapability],
    hint: Option<&str>,
) -> (ModeName, Mode) {
    let mode = Mode {
        capabilities: BASE
            .iter()
            .chain(extra)
            .cloned()
            .chain(security.iter().map(|capability| capability.capability()))
            .collect::<CapabilitySet>()
            .try_into()
            .expect("no embedded mode lists interaction"),
        instructions: Some(Instructions::Template(
            format!("security/{name}.md")
                .parse()
                .expect("embedded mode name is nonblank"),
        )),
        hint: hint.map(|hint| hint.parse().expect("embedded mode hint is nonblank")),
    };
    (self::name(name), mode)
}

/// The embedded modes, each replaced whole by a `configured` mode of its name.
pub(crate) fn configured(configured: &IndexMap<ModeName, Mode>) -> IndexMap<ModeName, Mode> {
    let mut modes = modes();
    modes.extend(configured.clone());
    modes
}

/// The embedded modes.
pub(crate) fn modes() -> IndexMap<ModeName, Mode> {
    use Capability::{Exec, Network, Write};
    use SecurityCapability::{Research, Scheduled, Test, Verdict};

    [
        mode("orchestrate", &[Write, Exec, Network], &SecurityCapability::ALL, None),
        mode("target", &[Network], &[Research],
            Some("Select the research target, record its deployment premises, and map the code into components"),
        ),
        mode("paths", &[Exec, Network], &[Research],
            Some("Record each entry an actor can drive as a path through the stages handling it, with the states its code reads and writes"),
        ),
        mode("entries", &[Exec, Network], &[Research],
            Some("Record one file's or interface's entries as paths, the input states taken in there and the states its code reads or writes, with their accesses"),
        ),
        mode("sweep", &[], &[Research, Scheduled],
            Some("Sweep paths once, splitting a long path into stretches: their flaws, premises, the states flaws leave wrong, and candidates"),
        ),
        mode("check", &[], &[Research, Scheduled],
            Some("Check states across calls: every writer against every reader, in every order the caller may drive them, recording the flaws that leave a state wrong"),
        ),
        mode("sweep-part", &[], &[Research],
            Some("Sweep one stretch of a long path: its flaws, the premises they rely on, and its candidates"),
        ),
        mode("probe", &[Write, Exec, Network], &[Research, Scheduled],
            Some("Sweep one path without code once, by disassembly or observation: its flaws, premises and candidates"),
        ),
        mode("settle", &[Exec, Network], &[Research, Scheduled],
            Some("Settle premises from the code, or review the loose ends of undecided ones"),
        ),
        mode("chain", &[Exec, Network], &[Research, Scheduled],
            Some("Chain flaws into candidates"),
        ),
        mode("validate", &[Exec, Network], &[Research, Scheduled, Verdict],
            Some("Independently score and review one candidate: examine what it rests on, then record a verdict"),
        ),
        mode("trace", &[Exec, Network], &[Research],
            Some("Trace one candidate through the source from the attacker's entry to its effect, recording where the input stops"),
        ),
        mode("environment", &[Write, Exec, Network], &[Research],
            Some("Prepare and record an approved, reproducible environment for live work"),
        ),
        mode("poc", &[Write, Exec, Network], &[Research, Scheduled, Test],
            Some("Test a candidate in an authorized environment: demonstrate a validated one, or observe the code premises an unproven one waits on"),
        ),
        mode("report", &[Write, Exec, Network], &[Research, Scheduled, Verdict],
            Some("Write one root flaw's evidence-based report, covering its group's validated scenarios, citing its tests"),
        ),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use skyhook::config::mode::Edition;

    use super::*;
    use crate::store::LivePolicy;

    /// The embedded modes as an agent entering each renders it in a run under `live`: the
    /// root its own, a child each other.
    pub(crate) fn editions(live: LivePolicy) -> IndexMap<ModeName, Edition> {
        let prompts = crate::prompts::prompts(live);
        let workspace = skyhook_worker::op::TargetPath::from("/workspace");
        let edition = |(name, mode): (ModeName, Mode)| {
            let agent = skyhook::prompts::Agent {
                depth: usize::from(name != root()),
                available_depth: 1,
                capabilities: mode.capabilities.set(),
                mode: Some(&name),
                workspace: &workspace,
                target: &skyhook_tools::target::TargetRef::Root,
                definition: None,
                facts: None,
            };
            let edition = prompts.edition(&mode, &agent).unwrap();
            (name, edition)
        };
        modes().into_iter().map(edition).collect()
    }
}
