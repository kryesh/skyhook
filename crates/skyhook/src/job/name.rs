//! The name a job shows.
use serde_json::Value;
use skyhook_base::string_newtype;

/// Lowercase kebab-case: the one definition behind the schema a model sees and
/// [`JobName`]'s check.
pub(crate) const NAME_PATTERN: &str = "^[a-z][a-z0-9]*(-[a-z0-9]+)*$";

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
