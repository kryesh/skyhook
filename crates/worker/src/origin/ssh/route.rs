//! The hops an SSH process passes through, as its origin receives them. Every
//! value is checked as it is built or decoded, so a route is safe to write as
//! OpenSSH configuration, and at most one hop types a secret by construction.
use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};
use skyhook_base::{enums::NamedEnum, string_newtype};

use crate::{origin::error::SshError, secret::Credential};

/// The hops of one SSH process, outermost first; its last hop is the destination.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SshRoute {
    /// No hop types a secret: agent keys and unencrypted key files only.
    Plain(Vec<Hop>),
    /// `hop` types `credential`: its key's passphrase when it names a key, else its
    /// password.
    Secret {
        before: Vec<Hop>,
        hop: Hop,
        credential: Credential,
        after: Vec<Hop>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Hop {
    /// Which target failures name.
    #[serde(rename = "name")]
    pub target: String,
    pub host: SshValue,
    pub port: Option<NonZeroU16>,
    pub user: Option<SshUser>,
    /// A private key on the origin, offered alone.
    pub key: Option<SshValue>,
    pub forward_agent: bool,
    pub host_keys: HostKeyChecking,
    /// Whether the hop takes the host's rotated keys, when the target says.
    pub host_key_updates: Option<HostKeyUpdates>,
    /// Written verbatim after Skyhook's own settings, which therefore win.
    pub options: Vec<(SshOptionName, SshValue)>,
}

/// OpenSSH's `StrictHostKeyChecking`: what a hop does with a host key its known-hosts
/// files lack. It never asks, since the terminal answers a hop's first prompt unread;
/// `ask`, OpenSSH's default, refuses the key as `yes` does.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq, NamedEnum)]
pub enum HostKeyChecking {
    #[default]
    #[serde(rename = "yes", alias = "true", alias = "ask")]
    Yes,
    #[serde(rename = "accept-new")]
    AcceptNew,
    #[serde(rename = "no", alias = "off", alias = "false")]
    No,
}

impl HostKeyChecking {
    /// An option's value, whose spelling OpenSSH reads in any case.
    pub fn of(value: &str) -> Option<Self> {
        Self::parse(&value.to_ascii_lowercase())
    }
}

/// OpenSSH's `UpdateHostKeys`: whether a hop records the keys a host rotates to. It
/// never asks, for the reason [`HostKeyChecking`] never does: `ask` declines them.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
pub enum HostKeyUpdates {
    #[serde(rename = "yes", alias = "true")]
    Yes,
    #[serde(rename = "no", alias = "false", alias = "ask")]
    No,
}

impl HostKeyUpdates {
    /// An option's value, whose spelling OpenSSH reads in any case.
    pub fn of(value: &str) -> Option<Self> {
        Self::parse(&value.to_ascii_lowercase())
    }
}

string_newtype! {
    /// Text an OpenSSH configuration line can hold: nonempty, without control characters.
    pub struct SshValue(SshError) = |value| {
        (!value.is_empty() && !value.chars().any(char::is_control))
            .then_some(())
            .ok_or(SshError::InvalidValue)
    };
}

string_newtype! {
    /// A user OpenSSH logs in as: one nonempty word, without control characters.
    pub struct SshUser(SshError) = |user| {
        let refused = |character: char| character.is_control() || character.is_whitespace();
        (!user.is_empty() && !user.chars().any(refused))
            .then_some(())
            .ok_or(SshError::InvalidUser)
    };
}

string_newtype! {
    /// An `ssh.options` key a configuration line can begin with: nonempty ASCII letters
    /// and digits.
    #[derive(PartialOrd, Ord)]
    pub struct SshOptionName(SshError) = |key| {
        (!key.is_empty() && key.bytes().all(|byte| byte.is_ascii_alphanumeric()))
            .then_some(())
            .ok_or(SshError::InvalidOptionName)
    };
}

/// How one hop authenticates.
pub(crate) enum HopAuth<'a> {
    /// Only keys already in the origin's agent.
    Agent,
    Key(&'a SshValue),
    /// A key whose passphrase the hop types.
    EncryptedKey(&'a SshValue),
    Password,
}

impl HopAuth<'_> {
    /// Whether the hop types its route's secret at a prompt.
    pub(crate) const fn prompts(&self) -> bool {
        matches!(self, Self::EncryptedKey(_) | Self::Password)
    }
}

impl Hop {
    fn unprompted(&self) -> HopAuth<'_> {
        match &self.key {
            Some(key) => HopAuth::Key(key),
            None => HopAuth::Agent,
        }
    }

    fn prompted(&self) -> HopAuth<'_> {
        match &self.key {
            Some(key) => HopAuth::EncryptedKey(key),
            None => HopAuth::Password,
        }
    }
}

impl SshRoute {
    /// Each hop, outermost first, with how it authenticates.
    pub(crate) fn hops(&self) -> impl Iterator<Item = (&Hop, HopAuth<'_>)> {
        let (before, secret, after) = match self {
            Self::Plain(hops) => (hops.as_slice(), None, &[][..]),
            Self::Secret {
                before, hop, after, ..
            } => (before.as_slice(), Some(hop), after.as_slice()),
        };
        (before.iter().map(|hop| (hop, hop.unprompted())))
            .chain(secret.map(|hop| (hop, hop.prompted())))
            .chain(after.iter().map(|hop| (hop, hop.unprompted())))
    }

    /// The hop that types a secret, and its credential.
    pub fn secret(&self) -> Option<(&Hop, &Credential)> {
        match self {
            Self::Plain(_) => None,
            Self::Secret {
                hop, credential, ..
            } => Some((hop, credential)),
        }
    }
}
