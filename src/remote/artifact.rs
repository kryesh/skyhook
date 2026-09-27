//! Embedded remote shims, named `<os>-<protocol>-<arch>` such as `linux-ssh-x86_64`.
use std::{borrow::Cow, fmt, sync::Arc};

use serde::Serialize;
use thiserror::Error;

use crate::{named_enum::named_enum, remote::error::DeploymentError};

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum Os {
        Linux = "linux",
    }
}

named_enum! {
    /// `uname -m` aliases parse to the canonical spelling.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum Arch {
        X86_64 = "x86_64" | "amd64",
        Aarch64 = "aarch64" | "arm64",
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum ShimProtocol {
        Ssh = "ssh",
    }
}

/// A remote host's operating system and architecture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}", self.os, self.arch)
    }
}

#[derive(Clone, Debug)]
pub struct EmbeddedShim {
    pub platform: Platform,
    pub protocol: ShimProtocol,
    pub bytes: Cow<'static, [u8]>,
}

/// Reads a named shim's bytes. Debug builds read shims from disk, so the catalog
/// only loads the one it deploys.
pub type ShimLoader = fn(&str) -> Option<Cow<'static, [u8]>>;

#[derive(Clone, Debug)]
struct ShimEntry {
    platform: Platform,
    protocol: ShimProtocol,
    name: Box<str>,
}

#[derive(Clone, Debug)]
pub struct EmbeddedShimCatalog {
    shims: Arc<[ShimEntry]>,
    load: ShimLoader,
}

impl Default for EmbeddedShimCatalog {
    fn default() -> Self {
        Self {
            shims: Arc::new([]),
            load: |_| None,
        }
    }
}

impl EmbeddedShimCatalog {
    /// Builds a catalog from `<os>-<protocol>-<arch>` artifact names that `load` reads.
    /// Names with an extension, such as stray build outputs, are skipped.
    pub fn from_embedded_assets<N: AsRef<str>>(
        names: impl IntoIterator<Item = N>,
        load: ShimLoader,
    ) -> Result<Self, ArtifactError> {
        let shims = names
            .into_iter()
            .filter(|name| std::path::Path::new(name.as_ref()).extension().is_none())
            .map(|name| {
                let name = name.as_ref();
                let invalid = || ArtifactError::InvalidName(name.to_owned());
                let [os, protocol, arch] = name.split('-').collect::<Vec<_>>()[..] else {
                    return Err(invalid());
                };
                Ok(ShimEntry {
                    platform: Platform::parse(os, arch).ok_or_else(invalid)?,
                    protocol: protocol.parse().map_err(|_| invalid())?,
                    name: name.into(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            shims: shims.into(),
            load,
        })
    }

    pub(crate) fn find(
        &self,
        protocol: ShimProtocol,
        platform: Platform,
    ) -> Result<EmbeddedShim, DeploymentError> {
        if self.shims.is_empty() {
            return Err(DeploymentError::NoShims);
        }
        let entry = self
            .shims
            .iter()
            .find(|shim| shim.protocol == protocol && shim.platform == platform)
            .ok_or(DeploymentError::NoShim { protocol, platform })?;
        let bytes = (self.load)(&entry.name).ok_or_else(|| DeploymentError::Unreadable {
            name: entry.name.clone(),
        })?;
        Ok(EmbeddedShim {
            platform,
            protocol,
            bytes,
        })
    }
}

impl EmbeddedShim {
    #[must_use]
    pub fn sha256(&self) -> String {
        crate::sha256_hex(&self.bytes)
    }

    #[must_use]
    pub fn installed_name(&self) -> String {
        format!("{}-{}", self.platform.os, self.protocol)
    }
}

#[derive(Clone, Debug, Error)]
pub enum ArtifactError {
    #[error("invalid shim artifact name `{0}`; expected <os>-<protocol>-<arch> in lowercase")]
    InvalidName(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(names: &[&str]) -> Result<EmbeddedShimCatalog, ArtifactError> {
        EmbeddedShimCatalog::from_embedded_assets(names, |_| Some(Cow::Borrowed(b"abc")))
    }

    #[test]
    fn finds_shims_by_platform_and_arch_aliases() {
        let shims = catalog(&[
            "linux-ssh-x86_64",
            "linux-ssh-aarch64",
            "linux-ssh-x86_64.d",
        ])
        .unwrap();
        let platform = Platform::parse("linux", "x86_64").unwrap();
        let shim = shims.find(ShimProtocol::Ssh, platform).unwrap();
        assert_eq!(
            (shim.platform.arch, &*shim.bytes),
            (Arch::X86_64, &b"abc"[..])
        );
        assert_eq!(shim.installed_name(), "linux-ssh");
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(shim.sha256(), sha);
        assert_eq!(platform.to_string(), "linux-x86_64");
        assert_eq!("arm64".parse::<Arch>().unwrap(), Arch::Aarch64);
        assert_eq!("amd64".parse::<Arch>().unwrap(), Arch::X86_64);
        assert!(Platform::parse("linux", "riscv64").is_none());
        assert!(Platform::parse("darwin", "x86_64").is_none());
        // An alias-named artifact serves the canonical platform.
        let aliased = catalog(&["linux-ssh-amd64"]).unwrap();
        assert!(aliased.find(ShimProtocol::Ssh, platform).is_ok());
    }

    #[test]
    fn find_distinguishes_missing_from_unreadable_shims() {
        let platform = Platform::parse("linux", "x86_64").unwrap();
        let find = |catalog: EmbeddedShimCatalog| catalog.find(ShimProtocol::Ssh, platform);
        assert!(matches!(
            find(EmbeddedShimCatalog::default()),
            Err(DeploymentError::NoShims)
        ));
        assert!(matches!(
            find(catalog(&["linux-ssh-aarch64"]).unwrap()),
            Err(DeploymentError::NoShim { .. })
        ));
        let unreadable =
            EmbeddedShimCatalog::from_embedded_assets(["linux-ssh-x86_64"], |_| None).unwrap();
        assert!(matches!(
            find(unreadable),
            Err(DeploymentError::Unreadable { name }) if &*name == "linux-ssh-x86_64"
        ));
    }

    #[test]
    fn rejects_invalid_names() {
        for name in [
            "",
            "linux-ssh-x86_64-extra",
            "linux--x86_64",
            "Linux-ssh-x86_64",
            "../linux-ssh-x86_64",
            "linux-ssh-$(uname)",
            "linux-ssh-riscv64",
            "linux-scp-x86_64",
            "darwin-ssh-x86_64",
        ] {
            assert!(
                matches!(catalog(&[name]), Err(ArtifactError::InvalidName(value)) if value == name),
                "accepted invalid name {name:?}"
            );
        }
    }
}
