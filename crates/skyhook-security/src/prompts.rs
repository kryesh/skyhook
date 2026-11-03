//! The persona and research modes' instructions: templates the library renders for each
//! agent, from its context and the run's live policy.

use std::sync::LazyLock;

use skyhook::prompts::{Prompts, Source, Templates};

use crate::store::LivePolicy;

const SOURCES: [(&str, Source); 29] = skyhook::templates!["security":
    Template "persona.md",
    Template "orchestrate.md",
    Template "target.md",
    Template "paths.md",
    Template "entries.md",
    Template "sweep.md",
    Template "sweep-part.md",
    Template "check.md",
    Template "probe.md",
    Template "question.md",
    Template "assess.md",
    Template "validate.md",
    Template "environment.md",
    Template "poc.md",
    Template "report.md",
    Template "recording.md",
    Template "sweep-rules.md",
    Template "cvss.md",
    Verbatim "scripts/scheduler.live.js",
    Verbatim "scripts/scheduler.no-live.js",
    Verbatim "scripts/paths.live.js",
    Verbatim "scripts/paths.no-live.js",
    Verbatim "scripts/target-inventory.js",
    Verbatim "scripts/target-map.js",
    Verbatim "scripts/sweep-measure.js",
    Verbatim "scripts/sweep-stretches.js",
    Verbatim "scripts/validate-gaps.js",
    Verbatim "scripts/poc-artifacts.js",
    Verbatim "scripts/report-scenarios.js",
];

/// What every template reads beside the agent's context: the run's settings, fixed for
/// the session.
#[derive(serde::Serialize)]
struct Context {
    /// Whether the run's policy allows live work.
    live: bool,
}

/// The prompts of a session whose run is under `live`.
pub fn prompts(live: LivePolicy) -> Prompts {
    static TEMPLATES: LazyLock<Templates<Context>> = LazyLock::new(|| {
        Templates::with(SOURCES, &Context { live: false }).expect("embedded templates parse")
    });
    let context = Context {
        live: live != LivePolicy::No,
    };
    let persona = "security/persona.md"
        .parse()
        .expect("template name is nonblank");
    let prompts = Prompts::host(&TEMPLATES, &context, persona);
    prompts.expect("the run's settings are a section")
}
