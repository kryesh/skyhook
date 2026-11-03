//! Embedded research modes. Named security.yaml modes replace these whole.

use indexmap::IndexMap;
use skyhook::tool::policy::{Capability, CapabilitySet, Instructions, Mode, ModeName};

use crate::capabilities::SecurityCapability;

/// The only root mode: every session's root orchestrates.
pub fn root() -> ModeName {
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
pub fn configured(configured: &IndexMap<ModeName, Mode>) -> IndexMap<ModeName, Mode> {
    let mut modes = modes();
    modes.extend(configured.clone());
    modes
}

/// The embedded modes.
pub fn modes() -> IndexMap<ModeName, Mode> {
    use Capability::{Exec, Network, Write};
    use SecurityCapability::{Research, Scheduled, Test, Verdict};

    [
        mode("orchestrate", &[Write, Exec, Network], &SecurityCapability::ALL, None),
        mode("target", &[Network], &[Research],
            Some("Select the research target, record its deployment premises, and map the code into components."),
        ),
        mode("paths", &[Exec, Network], &[Research],
            Some("Record each entry an actor can drive as a path through the stages handling it, with the states its code reads and writes."),
        ),
        mode("entries", &[Exec, Network], &[Research],
            Some("Record one file's or interface's entries as paths, the input states taken in there and the states its code reads or writes, with their accesses."),
        ),
        mode("sweep", &[], &[Research, Scheduled],
            Some("Sweep paths once, splitting a long path into stretches: their flaws, premises, the states flaws leave wrong, and candidates."),
        ),
        mode("check", &[], &[Research, Scheduled],
            Some("Check states across calls: every writer against every reader, in every order the caller may drive them, recording the flaws that leave a state wrong."),
        ),
        mode("sweep-part", &[], &[Research],
            Some("Sweep one stretch of a long path: its flaws, the premises they rely on, and its candidates."),
        ),
        mode("probe", &[Write, Exec, Network], &[Research, Scheduled],
            Some("Sweep one path without code once, by disassembly or observation: its flaws, premises and candidates."),
        ),
        mode("question", &[Exec, Network], &[Research, Scheduled],
            Some("Settle premises from the code, or chain flaws into candidates."),
        ),
        mode("assess", &[], &[Research, Scheduled],
            Some("Score one candidate's CVSS 4.0 vector were every premise to hold."),
        ),
        mode("validate", &[Exec, Network], &[Research, Scheduled, Verdict],
            Some("Independently review one candidate: examine what it rests on, then record a verdict."),
        ),
        mode("environment", &[Write, Exec, Network], &[Research],
            Some("Prepare and record an approved, reproducible environment for live work."),
        ),
        mode("poc", &[Write, Exec, Network], &[Research, Scheduled, Test],
            Some("Test a candidate in an authorized environment: demonstrate a validated one, or observe the code premises an unproven one waits on."),
        ),
        mode("report", &[Write, Exec, Network], &[Research, Scheduled, Verdict],
            Some("Write one root flaw's evidence-based report, covering its group's validated scenarios, citing its tests."),
        ),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use skyhook::tool::policy::Edition;

    use super::*;
    use crate::store::LivePolicy;

    /// The embedded modes as an agent entering each renders it in a run under `live`: the
    /// root its own, a child each other.
    pub(crate) fn editions(live: LivePolicy) -> IndexMap<ModeName, Edition> {
        let prompts = crate::prompts::prompts(live);
        let edition = |(name, mode): (ModeName, Mode)| {
            let agent = skyhook::prompts::Agent {
                depth: usize::from(name != root()),
                available_depth: 1,
                capabilities: mode.capabilities.set(),
                mode: Some(&name),
                workspace: std::path::Path::new("/workspace"),
                target: &skyhook::target::TargetRef::Root,
                definition: None,
            };
            let edition = prompts.edition(&mode, &agent).unwrap();
            (name, edition)
        };
        modes().into_iter().map(edition).collect()
    }

    /// The modes a template launches: a literal after `mode: `, or heading an entry of the
    /// scheduler's table (`["sweep", 2, …]`).
    fn launched(text: &str) -> Vec<&str> {
        let mut launched = Vec::new();
        for (at, _) in text.match_indices('"') {
            let Some((name, rest)) = text[at + 1..].split_once('"') else {
                break;
            };
            let named = text[..at].ends_with("mode: ")
                || (text[..at].ends_with('[')
                    && rest.strip_prefix(", ").is_some_and(|rest| {
                        rest.starts_with(|first: char| first.is_ascii_digit())
                    }));
            if named {
                launched.push(name);
            }
        }
        launched
    }

    /// Every mode a template launches is offered to the mode whose instructions hold it: it
    /// has a hint, and the parent's own grant holds each of its capabilities.
    #[test]
    fn every_template_launches_only_modes_its_parent_can_delegate() {
        let mut checked = Vec::new();
        for live in LivePolicy::ALL {
            let modes = editions(live);
            for (parent, mode) in &modes {
                let held = mode.capabilities.set();
                for child in launched(mode.instructions.as_ref().unwrap().as_str()) {
                    let launched = &modes[child];
                    assert!(launched.hint.is_some(), "{parent} launches {child}");
                    for capability in launched.capabilities.iter() {
                        assert!(
                            held.contains(capability),
                            "{parent} launches {child}, which needs {capability}"
                        );
                    }
                    checked.push(format!("{parent} {child}"));
                }
            }
        }
        for launch in [
            "paths entries",
            "sweep sweep-part",
            "orchestrate check",
            "orchestrate poc",
        ] {
            assert!(checked.iter().any(|checked| checked == launch), "{launch}");
        }
    }

    #[test]
    fn only_the_root_orchestrates_and_can_delegate_every_mode_and_none_grants_targets() {
        let modes = modes();
        assert!(modes[&root()].hint.is_none());
        let root = modes[&root()].capabilities.set();
        for (name, mode) in &modes {
            assert!(
                !mode.capabilities.set().contains(&Capability::Targets),
                "{name}"
            );
            assert!(
                mode.capabilities
                    .iter()
                    .all(|capability| root.contains(capability)),
                "{name}"
            );
        }
    }
}
