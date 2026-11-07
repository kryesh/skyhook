//! Typed execution targets, configuration, and routing.

mod config;
mod registry;
mod router;

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::newtype::string_newtype;

pub use config::{SshAuth, SshOptions, TargetAuth, TargetConfig, TargetsConfig, Transport};
pub(crate) use registry::{Revisions, TargetRegistry, replaces};
pub use registry::{TargetDefinition, TargetEdge, TargetError, TargetField, TargetSource};
pub(crate) use router::{Route, TargetRouter, select_location};

pub(crate) const MAX_TARGET_NAME_BYTES: usize = 128;

/// A machine's time zone as it read it: the IANA name of the zone its clock follows,
/// when it has one, and its offset from UTC in seconds then.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TimeZone {
    pub(crate) name: Option<String>,
    pub(crate) offset: i32,
}

/// Where the clock reads a zone `TZ` names, as `chrono::Local` looks for it.
const ZONE_DIRECTORIES: [&str; 4] = [
    "/usr/share/zoneinfo",
    "/share/zoneinfo",
    "/etc/zoneinfo",
    "/usr/share/lib/zoneinfo",
];

impl TimeZone {
    /// This machine's zone now, named as the clock that gives its offset selects it.
    pub(crate) fn local() -> Self {
        let tz = std::env::var("TZ").ok();
        let name = zone_name(tz.as_deref(), is_zone, || {
            iana_time_zone::get_timezone().ok()
        });
        let offset = chrono::Local::now().offset().local_minus_utc();
        Self { name, offset }
    }

    /// The offset as `±hh:mm`.
    pub(crate) fn offset(&self) -> String {
        let sign = if self.offset < 0 { '-' } else { '+' };
        let seconds = self.offset.unsigned_abs();
        format!("{sign}{:02}:{:02}", seconds / 3600, seconds % 3600 / 60)
    }
}

/// The zone `tz`, the `TZ` value, selects: the zone it names, `:` aside; none when it
/// gives a rule or nothing rather than a name; the `system` zone without it.
fn zone_name(
    tz: Option<&str>,
    is_zone: impl Fn(&str) -> bool,
    system: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let Some(tz) = tz else {
        return system();
    };
    let name = tz.strip_prefix(':').unwrap_or(tz);
    is_zone(name).then(|| name.to_owned())
}

/// Whether `name` is a zone of the tz database the clock reads.
fn is_zone(name: &str) -> bool {
    let relative = std::path::Path::new(name);
    let within = relative
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)));
    !name.is_empty()
        && within
        && ZONE_DIRECTORIES
            .iter()
            .any(|directory| std::path::Path::new(directory).join(relative).is_file())
}

string_newtype! {
    /// The name of a configured or session-added target
    #[derive(PartialOrd, Ord)]
    pub struct TargetName(TargetError) = |name| {
        if name == TargetRef::ROOT {
            return Err(TargetError::ReservedName);
        }
        let valid = !name.is_empty()
            && name.len() <= MAX_TARGET_NAME_BYTES
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        valid
            .then_some(())
            .ok_or(TargetError::InvalidName)
    }, schema = schemars::json_schema!({
        "type": "string",
        "pattern": format!("^[A-Za-z0-9_.-]{{1,{MAX_TARGET_NAME_BYTES}}}$"),
    });
}

// A path on an execution target, as tool arguments and results name one. An
// omitted target is the caller's own location; a named one resolves with the
// same rules as a tool's `target` argument. The `target` property is offered
// only with target selection, so schemas add it where that capability applies.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetPath {
    /// File path
    pub path: String,
    #[serde(default)]
    #[schemars(skip)]
    pub target: Option<TargetRef>,
}

impl TargetPath {
    /// Where schemas define this type.
    pub(crate) const SCHEMA: &'static str = "/$defs/TargetPath";

    /// The `target` property schemas add where the caller can select targets.
    /// It names the host holding a path, which need not be where the operation
    /// runs, so its wording differs from [`TargetRef::schema`]. Arguments also
    /// accept null for an omitted target; results omit it.
    pub(crate) fn target_schema(contract: &schemars::generate::Contract) -> serde_json::Value {
        let types = if contract.is_deserialize() {
            serde_json::json!(["string", "null"])
        } else {
            serde_json::json!("string")
        };
        serde_json::json!({
            "type": types,
            "description": "Target holding the path; omitted means your own"
        })
    }
}

/// An execution target: the session host running Skyhook, or a named target.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(try_from = "String", into = "String")]
pub enum TargetRef {
    Root,
    Named(TargetName),
}

impl TargetRef {
    /// The spelling of the session host wherever targets are named by text.
    const ROOT: &'static str = "root";

    /// The `target` property through which a caller that can select targets
    /// chooses where an operation runs; omitted, it runs where the caller does.
    pub(crate) fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": ["string", "null"],
            "description": "Execution target"
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Root => Self::ROOT,
            Self::Named(name) => name.as_str(),
        }
    }

    pub fn name(&self) -> Option<&TargetName> {
        match self {
            Self::Root => None,
            Self::Named(name) => Some(name),
        }
    }
}

impl schemars::JsonSchema for TargetRef {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        String::schema_name()
    }
    fn schema_id() -> std::borrow::Cow<'static, str> {
        String::schema_id()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        String::json_schema(generator)
    }
}

impl From<TargetName> for TargetRef {
    fn from(name: TargetName) -> Self {
        Self::Named(name)
    }
}

impl TryFrom<String> for TargetRef {
    type Error = TargetError;

    fn try_from(target: String) -> Result<Self, TargetError> {
        if target == Self::ROOT {
            Ok(Self::Root)
        } else {
            TargetName::try_from(target).map(Self::Named)
        }
    }
}

impl FromStr for TargetRef {
    type Err = TargetError;

    fn from_str(target: &str) -> Result<Self, TargetError> {
        Self::try_from(target.to_owned())
    }
}

impl From<TargetRef> for String {
    fn from(target: TargetRef) -> Self {
        match target {
            TargetRef::Root => TargetRef::ROOT.to_owned(),
            TargetRef::Named(name) => name.into(),
        }
    }
}

impl fmt::Display for TargetRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn names_are_validated_and_root_is_reserved() {
        assert_eq!("root".parse::<TargetRef>().unwrap(), TargetRef::Root);
        let named = "build.host-1_a".parse::<TargetRef>().unwrap();
        assert_eq!(named.as_str(), "build.host-1_a");
        assert_matches!("root".parse::<TargetName>(), Err(TargetError::ReservedName));
        for invalid in [
            "",
            "has space",
            "ünïcode",
            &"x".repeat(MAX_TARGET_NAME_BYTES + 1),
        ] {
            assert_matches!(
                invalid.parse::<TargetName>(),
                Err(TargetError::InvalidName),
                "{invalid:?}"
            );
            assert!(invalid.parse::<TargetRef>().is_err(), "{invalid:?}");
        }
        assert_eq!(
            serde_json::to_value([TargetRef::Root, named.clone()]).unwrap(),
            serde_json::json!(["root", "build.host-1_a"])
        );
        assert!(serde_json::from_value::<TargetName>(serde_json::json!("root")).is_err());
        assert_eq!(
            serde_json::from_value::<TargetRef>(serde_json::json!("root")).unwrap(),
            TargetRef::Root
        );
    }

    /// `TZ` selects the zone it names, as the clock does, and only an offset when it
    /// gives a rule; without it the system's zone is named.
    #[test]
    fn tz_names_the_zone_the_clock_follows() {
        let known = |name: &str| name == "Asia/Tokyo";
        let system = || Some("Etc/UTC".to_owned());
        let tokyo = Some("Asia/Tokyo".to_owned());
        for (tz, expected) in [
            (Some("Asia/Tokyo"), tokyo.clone()),
            (Some(":Asia/Tokyo"), tokyo),
            (Some("JST-9"), None),
            (Some(""), None),
            (None, Some("Etc/UTC".to_owned())),
        ] {
            assert_eq!(zone_name(tz, known, system), expected, "{tz:?}");
        }
        let nameless = TimeZone {
            name: None,
            offset: -(9 * 3600 + 30 * 60),
        };
        assert_eq!(nameless.offset(), "-09:30");
    }
}
