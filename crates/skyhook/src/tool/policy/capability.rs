//! Built-in and namespaced host permissions, including their stable wire names.

use skyhook_tools::diagnostic::{DiagnosticViewer, TargetView};
use std::{collections::BTreeSet, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use skyhook_base::string_newtype;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("host capability must be namespace.name, each component matching [a-z][a-z0-9_]*")]
pub struct InvalidHostCapability;

fn valid_component(component: &str) -> bool {
    component
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_lowercase)
        && component
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

string_newtype! {
    /// A host-defined permission, named `namespace.name`.
    #[derive(PartialOrd, Ord)]
    pub struct HostCapability(InvalidHostCapability) = |name| name
        .split_once('.')
        .is_some_and(|(namespace, name)| valid_component(namespace) && valid_component(name))
        .then_some(())
        .ok_or(InvalidHostCapability);
}

/// A name that is neither a built-in capability nor one of the host's, with every
/// capability valid in its place.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown capability `{name}`, expected one of {}", skyhook_base::decode::one_of(self.valid()))]
pub struct UnknownCapability {
    pub name: String,
    /// The host capabilities valid in its place.
    pub declared: Vec<HostCapability>,
}

impl UnknownCapability {
    fn valid(&self) -> impl Iterator<Item = &str> {
        let builtin = Capability::ALL.iter().map(Capability::as_str);
        builtin.chain(self.declared.iter().map(HostCapability::as_str))
    }
}

/// Why a name is not a capability. Every host capability's name has a namespace,
/// so a name without one is a built-in capability or none.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCapability {
    #[error(transparent)]
    Unknown(#[from] UnknownCapability),
    #[error(transparent)]
    Host(#[from] InvalidHostCapability),
}

/// A host capability and the title the user sees it under; see [`Capability::title`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCapabilityDeclaration {
    pub name: HostCapability,
    pub title: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Capability {
    Read,
    Write,
    Exec,
    Network,
    Targets,
    Agents,
    Interactive,
    Mcp,
    Host(HostCapability),
}

impl Capability {
    /// Every built-in capability. Host capabilities are declared separately by each host.
    pub const ALL: [Self; 8] = [
        Self::Read,
        Self::Write,
        Self::Exec,
        Self::Network,
        Self::Targets,
        Self::Agents,
        Self::Interactive,
        Self::Mcp,
    ];

    pub fn as_str(&self) -> &str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exec => "exec",
            Self::Network => "network",
            Self::Targets => "targets",
            Self::Agents => "agents",
            Self::Interactive => "interactive",
            Self::Mcp => "mcp",
            Self::Host(name) => name.as_str(),
        }
    }

    /// What the user sees this capability as. A host capability takes its title from
    /// `declared`; one declared elsewhere, such as another host's, keeps its name.
    pub fn title<'a>(&'a self, declared: &'a [HostCapabilityDeclaration]) -> &'a str {
        match self {
            Self::Read => "Read files",
            Self::Write => "Write files",
            Self::Exec => "Run commands",
            Self::Network => "Fetch from the network",
            Self::Targets => "Use remote targets",
            Self::Agents => "Start child agents",
            Self::Interactive => "Ask you questions",
            Self::Mcp => "Use MCP tools",
            Self::Host(name) => declared
                .iter()
                .find(|declaration| declaration.name == *name)
                .map_or(name.as_str(), |declaration| declaration.title),
        }
    }
}

skyhook_base::decode_from_str!(Capability);

impl FromStr for Capability {
    type Err = InvalidCapability;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        if let Some(builtin) = Self::ALL
            .into_iter()
            .find(|builtin| builtin.as_str() == name)
        {
            return Ok(builtin);
        }
        match name.contains('.') {
            true => Ok(Self::Host(name.parse()?)),
            false => Err(UnknownCapability {
                name: name.to_owned(),
                declared: Vec::new(),
            }
            .into()),
        }
    }
}

impl TryFrom<String> for Capability {
    type Error = InvalidCapability;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        name.parse()
    }
}

impl From<Capability> for String {
    fn from(capability: Capability) -> Self {
        match capability {
            Capability::Host(name) => name.into(),
            builtin => builtin.as_str().to_owned(),
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The host capabilities among `capabilities` that `declared` does not name.
pub fn undeclared<'a>(
    capabilities: impl IntoIterator<Item = &'a Capability>,
    declared: impl Fn(&HostCapability) -> bool,
) -> impl Iterator<Item = &'a HostCapability> {
    (capabilities.into_iter()).filter_map(move |capability| match capability {
        Capability::Host(name) if !declared(name) => Some(name),
        _ => None,
    })
}

/// A set of capabilities; it serialises as its sorted list.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, skyhook_base::decode::Decode,
)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    /// Construct an exact empty set, unlike the enabled-by-default session set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(BTreeSet::new())
    }

    pub fn contains(&self, capability: &Capability) -> bool {
        self.0.contains(capability)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn insert(&mut self, capability: Capability) {
        self.0.insert(capability);
    }

    pub fn remove(&mut self, capability: &Capability) {
        self.0.remove(capability);
    }

    #[must_use]
    pub fn for_agent(&self, available_depth: usize) -> Self {
        let mut capabilities = self.clone();
        if available_depth == 0 {
            capabilities.remove(&Capability::Agents);
        }
        capabilities
    }

    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.0.iter()
    }

    /// The target as a viewer holding these capabilities may see it: only one
    /// that can select targets learns which target anything ran on.
    pub fn visible_target<'a>(
        &self,
        target: &'a skyhook_tools::target::TargetRef,
    ) -> Option<&'a skyhook_tools::target::TargetRef> {
        self.contains(&Capability::Targets).then_some(target)
    }

    /// How a viewer holding these capabilities sees targets in a diagnostic.
    pub fn target_view(&self) -> TargetView {
        match self.contains(&Capability::Targets) {
            true => TargetView::Named,
            false => TargetView::Hidden,
        }
    }
}

/// Inspection by capabilities alone is a host view.
impl From<&CapabilitySet> for DiagnosticViewer<'_> {
    fn from(capabilities: &CapabilitySet) -> Self {
        capabilities.target_view().into()
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self(BTreeSet::from([
            Capability::Read,
            Capability::Write,
            Capability::Exec,
            Capability::Network,
            Capability::Agents,
            Capability::Interactive,
            Capability::Mcp,
        ]))
    }
}

impl std::ops::BitAnd for &CapabilitySet {
    type Output = CapabilitySet;

    fn bitand(self, other: Self) -> CapabilitySet {
        CapabilitySet(&self.0 & &other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_parse_builtin_and_valid_host_names_only() {
        let host: Capability = "host.review".parse().unwrap();
        assert_eq!(host, Capability::Host("host.review".parse().unwrap()));
        for capability in Capability::ALL.into_iter().chain([host]) {
            let json = serde_json::to_value(&capability).unwrap();
            assert_eq!(json, capability.as_str());
            assert_eq!(
                serde_json::from_value::<Capability>(json).unwrap(),
                capability
            );
        }
        for name in [
            "",
            "read.",
            "Host.name",
            "host.name.more",
            "host-name.action",
        ] {
            assert!(name.parse::<HostCapability>().is_err(), "{name:?}");
        }
    }

    /// A name without a namespace can only be a built-in capability, so one that is
    /// not is unknown rather than a malformed host capability.
    #[test]
    fn a_plain_name_is_built_in_or_unknown_and_a_dotted_one_is_a_host_capability() {
        for name in ["wirte", "Read", ""] {
            assert_eq!(
                name.parse::<Capability>(),
                Err(InvalidCapability::Unknown(UnknownCapability {
                    name: name.into(),
                    declared: Vec::new(),
                }))
            );
        }
        let builtins = skyhook_base::decode::one_of(Capability::ALL.iter().map(Capability::as_str));
        assert_eq!(
            "wirte".parse::<Capability>().unwrap_err().to_string(),
            format!("unknown capability `wirte`, expected one of {builtins}")
        );
        for name in ["read.", "Host.name", "host.name.more"] {
            assert_eq!(
                name.parse::<Capability>(),
                Err(InvalidCapability::Host(InvalidHostCapability))
            );
        }
    }
}
