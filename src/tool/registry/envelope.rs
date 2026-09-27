//! The execution facts a call carries beside its handler arguments: whether it
//! runs in the background, the name its job shows, and the target it selects.

use serde_json::Value;

use super::{Arguments, RegisteredTool, ToolPlacement, ToolSpec};
use crate::{
    newtype::string_newtype,
    target::TargetRef,
    tool::{
        AdmissionError,
        diagnostic::{Operation, Subject},
    },
};

pub(crate) const BACKGROUND: &str = "bg";
pub(super) const NAME: &str = "name";
pub const TARGET: &str = "target";
/// The script namespace for job controls.
pub(super) const JOB: &str = "job";

/// Lowercase kebab-case: the one definition behind the schema a model sees and
/// [`JobName`]'s check.
const NAME_PATTERN: &str = "^[a-z][a-z0-9]*(-[a-z0-9]+)*$";

string_newtype! {
    /// A job's display name: a lowercase letter first, then `a-z`, `0-9`, and
    /// single hyphens between nonempty words.
    pub struct JobName(InvalidJobName) = |name| {
        static PATTERN: std::sync::LazyLock<jsonschema::Validator> =
            std::sync::LazyLock::new(|| {
                let pattern = serde_json::json!({"pattern": NAME_PATTERN});
                jsonschema::validator_for(&pattern).expect("the name pattern compiles")
            });
        (PATTERN.is_valid(&Value::from(name)))
            .then_some(())
            .ok_or(InvalidJobName)
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "name must be lowercase kebab-case: start with a letter, use only a-z, 0-9, and single hyphens between nonempty words"
)]
pub struct InvalidJobName;

pub(super) fn name_schema() -> Value {
    serde_json::json!({
        "type": ["string", "null"],
        "pattern": NAME_PATTERN,
        "description": "Lowercase kebab-case name shown in job state and notifications."
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

/// Take the envelope out of `arguments`, leaving the handler's own. Each part is
/// admitted only where the tool declares it; `target` is otherwise rejected.
pub(crate) fn split_envelope(
    spec: &ToolSpec,
    tool: &RegisteredTool,
    arguments: &mut Arguments,
) -> Result<ExecutionEnvelope, AdmissionError> {
    let background = match arguments.remove(BACKGROUND) {
        None => false,
        Some(Value::Bool(value)) if spec.supports_background => value,
        Some(Value::Bool(_)) => {
            return Err(AdmissionError::invalid_arguments(
                "background execution is unsupported",
            ));
        }
        Some(_) => return Err(AdmissionError::invalid_arguments("bg must be a boolean")),
    };
    let name = if tool.execution.supports_name {
        match arguments.remove(NAME) {
            None | Some(Value::Null) => None,
            Some(Value::String(name)) => {
                Some(JobName::try_from(name).map_err(AdmissionError::invalid_arguments)?)
            }
            Some(_) => return Err(AdmissionError::invalid_arguments(InvalidJobName)),
        }
    } else {
        None
    };
    // A host tool's `target`, if it declares one, is its own argument.
    let target = match tool.placement() {
        ToolPlacement::TargetedWorkspace => match arguments.remove(TARGET) {
            None | Some(Value::Null) => None,
            Some(Value::String(target)) => Some(target.parse::<TargetRef>().map_err(|error| {
                error
                    .into_admission_error()
                    .operation(Operation::Validate, Subject::argument([TARGET]))
            })?),
            Some(_) => return Err(AdmissionError::invalid_arguments("target must be a string")),
        },
        ToolPlacement::InheritWorkspace if arguments.contains_key(TARGET) => {
            return Err(AdmissionError::invalid_arguments(format!(
                "tool `{}` does not accept a target",
                tool.name()
            )));
        }
        ToolPlacement::InheritWorkspace | ToolPlacement::Host => None,
    };
    Ok(ExecutionEnvelope {
        launch: JobLaunch { background, name },
        target,
    })
}
