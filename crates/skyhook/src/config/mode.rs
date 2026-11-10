//! Modes: named permission presets, as configured and as an agent runs them.
use serde::{Deserialize, Serialize};
use skyhook_base::{
    newtype::{Blank, Prose, nonblank},
    string_newtype,
};

use crate::tool::policy::{Capability, CapabilitySet, ModeCapabilities};

string_newtype! {
    /// The name a mode is declared under and selected by.
    #[derive(PartialOrd, Ord)]
    pub struct ModeName(Blank) = |name| nonblank("mode name", name);
}

/// What an agent running in a mode is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Instructions {
    /// Configured text, used as written.
    Text(Prose),
    /// A template of the host's prompts, rendered for each agent that enters the mode.
    Template(crate::prompts::TemplateName),
}

impl std::str::FromStr for Instructions {
    type Err = Blank;

    fn from_str(text: &str) -> Result<Self, Blank> {
        text.parse().map(Self::Text)
    }
}

/// Configuration spells only text.
impl<'de> Deserialize<'de> for Instructions {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Prose::deserialize(deserializer).map(Self::Text)
    }
}

/// A named permission preset. Interaction is supplied by the runtime host, never listed.
/// Configured, its instructions may be a host template; an agent runs its [`Edition`].
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, bound(deserialize = "I: Deserialize<'de>"))]
pub struct Mode<I = Instructions> {
    pub capabilities: ModeCapabilities,
    /// Added to the system prompt of an agent running in the mode.
    #[serde(default)]
    pub instructions: Option<I>,
    /// Describes the mode to agents choosing one for a child; without it they cannot.
    #[serde(default)]
    pub hint: Option<Prose>,
}

/// A mode as an agent entering it runs it, and the journal keeps it: its instructions
/// as they rendered for that agent.
pub type Edition = Mode<Prose>;

/// Instructions as they are written out.
pub trait Written {
    /// Their text; a host's template is not configuration, so it has none.
    fn written(&self) -> Option<&Prose>;
}

impl Written for Prose {
    fn written(&self) -> Option<&Prose> {
        Some(self)
    }
}

impl Written for Instructions {
    fn written(&self) -> Option<&Prose> {
        match self {
            Self::Text(text) => Some(text),
            Self::Template(_) => None,
        }
    }
}

impl<I: Written> Serialize for Mode<I> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Spelled<'a> {
            capabilities: &'a ModeCapabilities,
            #[serde(skip_serializing_if = "Option::is_none")]
            instructions: Option<&'a Prose>,
            #[serde(skip_serializing_if = "Option::is_none")]
            hint: Option<&'a Prose>,
        }
        let spelled = Spelled {
            capabilities: &self.capabilities,
            instructions: self.instructions.as_ref().and_then(I::written),
            hint: self.hint.as_ref(),
        };
        spelled.serialize(serializer)
    }
}

impl<I> Mode<I> {
    /// What the mode grants an agent under `ceiling` that may delegate
    /// `available_depth` deep: its capabilities within the ceiling, interaction
    /// wherever the ceiling has it, and no agents without depth to give.
    #[must_use]
    pub fn grants(&self, ceiling: &CapabilitySet, available_depth: usize) -> CapabilitySet {
        let mut granted = self.capabilities.set() & ceiling;
        if ceiling.contains(&Capability::Interactive) {
            granted.insert(Capability::Interactive);
        }
        granted.for_agent(available_depth)
    }
}
