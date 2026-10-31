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

/// Prompt files joined, each followed by its separator. A file's trailing newlines are not
/// part of it, so only the separators decide the text between parts.
fn joined(parts: &[(&str, &str)]) -> String {
    (parts.iter())
        .flat_map(|(part, separator)| [part.trim_end_matches('\n'), separator])
        .collect()
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
            Some("Select the research target, record its deployment premises, and map the code into components."),
        )),
        ("paths", mode(
            &[], &[Research],
            include_str!("prompts/paths.md"),
            Some("Inventory the entry points an actor can drive and group them into paths that share one flow."),
        )),
        ("sources-file", mode(
            &[], &[Research],
            include_str!("prompts/sources-file.md"),
            Some("Record one file's entry points as sources, each on a path of its own up to its handoff."),
        )),
        ("sweep", mode(
            &[], &[Research],
            &joined(&[
                (include_str!("prompts/sweep-rules.md"), "\n\n"),
                (include_str!("prompts/path-sweep.md"), "\n"),
            ]),
            Some("Sweep one path once, splitting a long one into stretches: its flaws, the premises they and its flow rely on, and its candidates."),
        )),
        ("sweep-part", mode(
            &[], &[Research],
            &joined(&[
                (include_str!("prompts/sweep-rules.md"), "\n\n"),
                (include_str!("prompts/sweep-part.md"), "\n"),
            ]),
            Some("Sweep one stretch of a long path: its flaws, the premises they rely on, and its candidates."),
        )),
        ("question", mode(
            &[Network], &[Research],
            include_str!("prompts/question.md"),
            Some("Settle premises from the code, or chain flaws into candidates."),
        )),
        ("assess", mode(
            &[], &[Research],
            &joined(&[
                (include_str!("prompts/assess.md"), "\n\n"),
                (include_str!("prompts/cvss.md"), "\n"),
            ]),
            Some("Score one candidate's CVSS 4.0 vector were every premise to hold."),
        )),
        ("validate", mode(
            &[Exec, Network], &[Research, Verdict],
            &joined(&[
                (include_str!("prompts/validate.md"), "\n\n"),
                (include_str!("prompts/cvss.md"), "\n"),
            ]),
            Some("Independently review one candidate: settle what it rests on, then record a verdict."),
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
            Some("Write one root flaw's evidence-based report, covering its group's validated scenarios, citing its tests."),
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
