use skyhook::tool::policy::HostCapabilityDeclaration;
use skyhook::tool::policy::{Capability, HostCapability};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityCapability {
    Research,
    Verdict,
    Test,
    /// Begin the work the scheduler assigns: the modes it launches.
    Scheduled,
    /// Schedule the run and write its reports out: the root's alone.
    Coordinate,
}

impl SecurityCapability {
    pub const ALL: [Self; 5] = [
        Self::Research,
        Self::Verdict,
        Self::Test,
        Self::Scheduled,
        Self::Coordinate,
    ];

    pub fn host(self) -> HostCapability {
        match self {
            Self::Research => "security.research",
            Self::Verdict => "security.verdict",
            Self::Test => "security.test",
            Self::Scheduled => "security.scheduled",
            Self::Coordinate => "security.coordinate",
        }
        .parse()
        .expect("built-in security capability names are valid")
    }

    pub fn capability(self) -> Capability {
        Capability::Host(self.host())
    }

    /// How the sidebar names the capability.
    pub fn title(self) -> &'static str {
        match self {
            Self::Research => "Record research",
            Self::Verdict => "Decide verdicts",
            Self::Test => "Record tests",
            Self::Scheduled => "Begin scheduled work",
            Self::Coordinate => "Coordinate the run",
        }
    }
}

pub fn declarations() -> impl Iterator<Item = HostCapabilityDeclaration> {
    SecurityCapability::ALL
        .into_iter()
        .map(|capability| HostCapabilityDeclaration {
            name: capability.host(),
            title: capability.title(),
        })
}
