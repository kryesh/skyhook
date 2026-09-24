//! The execution facts a call carries beside its handler arguments: whether it
//! runs in the background, the name its job shows, and the target it selects.

use serde_json::Value;

use super::{RegisteredTool, ToolPlacement, ToolSpec};
use crate::{
    newtype::string_newtype,
    target::TargetRef,
    tool::{
        AdmissionError,
        diagnostic::{Operation, Subject},
    },
};

string_newtype! {
    /// A job's display name: a lowercase letter first, then `a-z`, `0-9`, and
    /// single hyphens between nonempty words.
    pub struct JobName(InvalidJobName) = |name| {
        let valid = name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && name.split('-').all(|word| {
                !word.is_empty()
                    && word
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            });
        valid.then_some(()).ok_or(InvalidJobName)
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "name must be lowercase kebab-case: start with a letter, use only a-z, 0-9, and single hyphens between nonempty words"
)]
pub struct InvalidJobName;

impl JobName {
    /// The name a call asked for, before any tool admitted it: what a failure
    /// with no job echoes back.
    pub fn requested(arguments: &serde_json::Map<String, Value>) -> Option<Self> {
        arguments
            .get("name")
            .and_then(Value::as_str)
            .and_then(|name| name.parse().ok())
    }
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
    mut arguments: Value,
) -> Result<(Value, ExecutionEnvelope), AdmissionError> {
    let object = arguments
        .as_object_mut()
        .ok_or(AdmissionError::arguments_must_be_object())?;
    let background = match object.remove("bg") {
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
        match object.remove("name") {
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
        ToolPlacement::TargetedWorkspace => match object.remove("target") {
            None | Some(Value::Null) => None,
            Some(Value::String(target)) => Some(target.parse::<TargetRef>().map_err(|error| {
                error
                    .into_admission_error()
                    .operation(Operation::Validate, Subject::argument(["target"]))
            })?),
            Some(_) => return Err(AdmissionError::invalid_arguments("target must be a string")),
        },
        ToolPlacement::InheritWorkspace if object.contains_key("target") => {
            return Err(AdmissionError::invalid_arguments(format!(
                "tool `{}` does not accept a target",
                tool.name()
            )));
        }
        ToolPlacement::InheritWorkspace | ToolPlacement::Host => None,
    };
    Ok((
        arguments,
        ExecutionEnvelope {
            launch: JobLaunch { background, name },
            target,
        },
    ))
}
