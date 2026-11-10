//! What a target reports about itself.
use std::fmt;

use serde::{Deserialize, Serialize};
use skyhook_base::enums::NamedEnum;

use crate::op::TargetPath;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Linux,
    Windows,
}

impl Os {
    /// The shell that runs shell text here, as an agent's context names it.
    pub const fn shell(self) -> &'static str {
        match self {
            Self::Linux => "sh",
            Self::Windows => "powershell-5.1",
        }
    }

    /// What an executable's file name ends with here.
    pub const fn executable_suffix(self) -> &'static str {
        match self {
            Self::Linux => "",
            Self::Windows => ".exe",
        }
    }
}

/// `uname -m` aliases parse to the canonical spelling.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    #[serde(alias = "amd64")]
    X86_64,
    #[serde(alias = "arm64")]
    Aarch64,
}

/// A machine's operating system and architecture.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    /// Parses an operating system and architecture spelling, such as trimmed
    /// lowercase `uname -s` and `uname -m` output.
    #[must_use]
    pub fn parse(os: &str, arch: &str) -> Option<Self> {
        Some(Self {
            os: os.parse().ok()?,
            arch: arch.parse().ok()?,
        })
    }

    /// This machine's.
    pub const fn current() -> Self {
        Self {
            os: if cfg!(windows) {
                Os::Windows
            } else {
                Os::Linux
            },
            arch: if cfg!(target_arch = "aarch64") {
                Arch::Aarch64
            } else {
                Arch::X86_64
            },
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}", self.os, self.arch)
    }
}

/// What a target reports about itself as a connection opens: what tools that run
/// there need of its environment.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Facts {
    /// Shell text runs in its operating system's [`Os::shell`].
    pub platform: Platform,
    pub zone: TimeZone,
    /// The connection's authorization root, as the target resolved it.
    pub authorization_root: TargetPath,
}

/// A machine's time zone as it read it: the IANA name of the zone its clock follows,
/// when it has one, and its offset from UTC in seconds then.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TimeZone {
    pub name: Option<String>,
    pub offset: i32,
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
    pub fn local() -> Self {
        let tz = std::env::var("TZ").ok();
        let name = zone_name(tz.as_deref(), is_zone, || {
            iana_time_zone::get_timezone().ok()
        });
        let offset = chrono::Local::now().offset().local_minus_utc();
        Self { name, offset }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platforms_parse_canonical_spellings_and_aliases() {
        let platform = Platform::parse("linux", "x86_64").unwrap();
        assert_eq!(platform.to_string(), "linux-x86_64");
        assert_eq!(Platform::parse("linux", "amd64"), Some(platform));
        assert_eq!("arm64".parse::<Arch>().unwrap(), Arch::Aarch64);
        assert!(Platform::parse("linux", "riscv64").is_none());
        assert!(Platform::parse("darwin", "x86_64").is_none());
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
    }
}
