//! Embedded research modes. Named security.yaml modes replace these whole.

use indexmap::IndexMap;
use skyhook::tool::policy::{Capability, CapabilitySet, Mode, ModeName};

use crate::{capabilities::SecurityCapability, store::PocPolicy};

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

fn mode(
    extra: &[Capability],
    security: &[SecurityCapability],
    instructions: &str,
    hint: Option<&str>,
) -> Mode {
    Mode {
        capabilities: BASE
            .iter()
            .chain(extra)
            .cloned()
            .chain(security.iter().map(|capability| capability.capability()))
            .collect::<CapabilitySet>()
            .try_into()
            .expect("no embedded mode lists interaction"),
        instructions: Some(
            instructions
                .parse()
                .expect("embedded instructions are nonblank"),
        ),
        hint: hint.map(|hint| hint.parse().expect("embedded mode hint is nonblank")),
    }
}

/// Mode instructions as a launch under `poc` reads them: a block from `<!-- if poc -->` to
/// `<!-- end -->` only while the policy may test, one from `<!-- if no poc -->` only while it
/// may not. The root's instructions change with the policy, so a launch under another policy
/// sends the reopened root a new system prompt.
pub fn rendered(text: &str, poc: PocPolicy) -> String {
    let testing = poc != PocPolicy::No;
    let mut kept = true;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        match line.trim_end() {
            "<!-- if poc -->" => kept = testing,
            "<!-- if no poc -->" => kept = !testing,
            "<!-- end -->" => kept = true,
            _ if kept => out.push_str(line),
            _ => {}
        }
    }
    out
}

/// `modes` with their instructions rendered for a launch under `poc`.
pub fn for_policy(mut modes: IndexMap<ModeName, Mode>, poc: PocPolicy) -> IndexMap<ModeName, Mode> {
    for mode in modes.values_mut() {
        mode.instructions = (mode.instructions.take())
            .and_then(|instructions| rendered(instructions.as_str(), poc).parse().ok());
    }
    modes
}

pub fn modes() -> IndexMap<ModeName, Mode> {
    use Capability::{Exec, Network, Write};
    use SecurityCapability::{Research, Test, Verdict};

    [
        ("orchestrate", mode(
            &[Write, Exec, Network], &SecurityCapability::ALL,
            include_str!("prompts/orchestrate.md"),
            None,
        )),
        ("target", mode(
            &[Network], &[Research],
            include_str!("prompts/target.md"),
            Some("Select the research target, its boundaries and assumptions, and map the code into components."),
        )),
        ("paths", mode(
            &[], &[Research],
            include_str!("prompts/paths.md"),
            Some("Inventory the entry points an actor can drive and group them into paths that share one interpretation."),
        )),
        ("sources-file", mode(
            &[], &[Research],
            include_str!("prompts/sources-file.md"),
            Some("Record one file's entry points as sources and name each one's handoff."),
        )),
        ("component", mode(
            &[], &[Research],
            concat!(include_str!("prompts/sweep.md"), "\n", include_str!("prompts/component.md")),
            Some("Sweep one component once, splitting a large one into parts: its flaws, guarantees, checks and unexamined regions."),
        )),
        ("component-part", mode(
            &[], &[Research],
            concat!(include_str!("prompts/sweep.md"), "\n", include_str!("prompts/component-part.md")),
            Some("Sweep one part of a large component: its flaws, local guarantees, checks and unexamined regions."),
        )),
        ("path-review", mode(
            &[], &[Research],
            concat!(
                include_str!("prompts/path-review.md"), " ", include_str!("prompts/exposure.md"),
                " ", include_str!("prompts/path-review-candidates.md"), "\n",
                include_str!("prompts/cvss.md"), "\n", include_str!("prompts/path-review-end.md"),
            ),
            Some("Review one path against the components it reaches, or assess one candidate."),
        )),
        ("question", mode(
            &[Network], &[Research],
            include_str!("prompts/question.md"),
            Some("Settle the questions or follow-ups assigned, each with its own outcome, and record what settles them."),
        )),
        ("validate", mode(
            &[Exec, Network], &[Research, Verdict],
            concat!(
                include_str!("prompts/validate.md"), "\n", include_str!("prompts/cvss.md"), "\n",
                include_str!("prompts/validate-verdict.md"), "\n",
                include_str!("prompts/exposure.md"), "\n\n", include_str!("prompts/validate-end.md"),
            ),
            Some("Independently review one candidate: settle its premises and routes, then record a verdict."),
        )),
        ("environment", mode(
            &[Write, Exec, Network], &[Research],
            include_str!("prompts/environment.md"),
            Some("Prepare and record an approved, reproducible test environment."),
        )),
        ("poc", mode(
            &[Write, Exec, Network], &[Research, Test],
            include_str!("prompts/poc.md"),
            Some("Demonstrate a validated candidate in an authorized environment and record the outcome."),
        )),
        ("report", mode(
            &[Write, Exec, Network], &[Verdict],
            include_str!("prompts/report.md"),
            Some("Write one root flaw's evidence-based report, covering its group's validated scenarios, with reproducible artifact references."),
        )),
    ]
    .into_iter()
    .map(|(key, mode)| (name(key), mode))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a launch that may test reads the orchestrator's PoC parts; no mode names the
    /// policy otherwise, and no marker is left.
    #[test]
    fn the_poc_parts_render_only_while_the_policy_may_test() {
        let instructions = |poc| {
            for_policy(modes(), poc)
                .into_values()
                .filter_map(|mode| mode.instructions)
                .map(|text| text.as_str().to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let (tested, untested) = (instructions(PocPolicy::Ask), instructions(PocPolicy::No));
        assert!(!tested.contains("<!--") && !untested.contains("<!--"));
        for part in ["## PoC gate", "pocSlots", "tests: {"] {
            assert!(tested.contains(part), "{part}");
            assert!(!untested.contains(part), "{part}");
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
