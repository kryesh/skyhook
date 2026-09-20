//! Decoding of the private JavaScript wrapper envelope produced by `runtime.js`.

use serde::Deserialize;
use serde_json::Value;

use crate::job::output::ScriptPresentation;

#[derive(Deserialize)]
pub(super) struct Envelope {
    pub(super) ok: bool,
    #[serde(default)]
    pub(super) value: Value,
    #[serde(default)]
    pub(super) presentation: ScriptPresentation,
    #[serde(default)]
    pub(super) error: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_round_trips_success_and_failure() {
        let success: Envelope =
            serde_json::from_str(r#"{"ok":true,"value":[null,42],"presentation":{}}"#).unwrap();
        assert!(success.ok);
        assert_eq!(success.value, json!([null, 42]));
        assert!(success.presentation.fields.is_empty() && success.error.is_null());
        let failure: Envelope =
            serde_json::from_str(r#"{"ok":false,"error":{"message":"boom"}}"#).unwrap();
        assert!(!failure.ok);
        assert_eq!(failure.error["message"], "boom");
    }
}
