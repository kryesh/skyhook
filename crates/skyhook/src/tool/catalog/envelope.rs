//! The execution facts a call carries beside its handler arguments: whether it
//! runs in the background, the name its job shows, and the target it selects.

use serde_json::Value;

use super::{Arguments, RegisteredTool, ToolPlacement, ToolSpec};
use crate::job::JobName;
use skyhook_tools::error::AdmissionError;
use skyhook_tools::target::TargetRef;
use skyhook_worker::diagnostic::{Operation, Subject};

pub const BACKGROUND: &str = "bg";
pub(super) const NAME: &str = "name";
pub const TARGET: &str = "target";

pub(super) fn name_schema() -> Value {
    serde_json::json!({
        "type": ["string", "null"],
        "pattern": crate::job::NAME_PATTERN,
        "description": "Lowercase kebab-case name shown in job state and notifications"
    })
}

/// How the call's job runs: in the background, under a display name.
pub struct JobLaunch {
    pub background: bool,
    pub name: Option<JobName>,
}

pub struct ExecutionEnvelope {
    pub launch: JobLaunch,
    pub target: Option<TargetRef>,
}

/// The envelope's parts a call names, as its schema admitted them.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct Envelope {
    bg: bool,
    name: Option<JobName>,
    /// Parsed apart, so an invalid name refuses the reference it spells.
    target: Option<String>,
}

/// Take the envelope out of `arguments`, leaving the handler's own. Each part is
/// admitted only where the tool declares it.
pub fn split_envelope(
    spec: &ToolSpec,
    tool: &RegisteredTool,
    arguments: &mut Arguments,
) -> Result<ExecutionEnvelope, AdmissionError> {
    let mut envelope = Arguments::new();
    let mut take = |key: &str| {
        if let Some(value) = arguments.remove(key) {
            envelope.insert(key.to_owned(), value);
        }
    };
    take(BACKGROUND);
    if tool.execution.supports_name {
        take(NAME);
    }
    // Any other tool's `target`, if it declares one, is its own argument.
    if tool.placement() == ToolPlacement::TargetedWorkspace {
        take(TARGET);
    }
    let Envelope { bg, name, target } = serde_json::from_value(Value::Object(envelope))
        .map_err(AdmissionError::invalid_arguments)?;
    if bg && !spec.supports_background {
        return Err(AdmissionError::invalid_arguments(
            "background execution is unsupported",
        ));
    }
    let target = target.map(|target| target.parse::<TargetRef>()).transpose();
    let target = target.map_err(|error| {
        AdmissionError::from(crate::target::TargetError::from(error))
            .operation(Operation::Validate, Subject::argument([TARGET]))
    })?;
    let launch = JobLaunch {
        background: bg,
        name,
    };
    Ok(ExecutionEnvelope { launch, target })
}
