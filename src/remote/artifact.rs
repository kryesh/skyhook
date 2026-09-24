//! Embedded remote shims, named `<os>-<protocol>-<arch>` such as `linux-ssh-x86_64`.
use std::{borrow::Cow, fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::named_enum::named_enum;

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
    pub enum Os {
        Linux = "linux",
    }
}

named_enum! {
    /// `uname -m` aliases parse to the canonical spelling.
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
    pub enum Arch {
        X86_64 = "x86_64" | "amd64",
        Aarch64 = "aarch64" | "arm64",
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
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

#[derive(Clone, Debug, Default)]
pub struct EmbeddedShimCatalog {
    shims: Arc<[EmbeddedShim]>,
}

impl EmbeddedShimCatalog {
    /// Builds a catalog from `<os>-<protocol>-<arch>` artifact names and their bytes.
    /// Names with an extension, such as stray build outputs, are skipped.
    pub fn from_embedded_assets<I, N>(assets: I) -> Result<Self, ArtifactError>
    where
        I: IntoIterator<Item = (N, Cow<'static, [u8]>)>,
        N: AsRef<str>,
    {
        let shims = assets
            .into_iter()
            .filter(|(name, _)| std::path::Path::new(name.as_ref()).extension().is_none())
            .map(|(name, bytes)| {
                let name = name.as_ref();
                let invalid = || ArtifactError::InvalidName(name.to_owned());
                let [os, protocol, arch] = name.split('-').collect::<Vec<_>>()[..] else {
                    return Err(invalid());
                };
                Ok(EmbeddedShim {
                    platform: Platform::parse(os, arch).ok_or_else(invalid)?,
                    protocol: protocol.parse().map_err(|_| invalid())?,
                    bytes,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            shims: shims.into(),
        })
    }

    /// Reports whether this build supplied any remote artifacts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shims.is_empty()
    }

    #[must_use]
    pub fn find(&self, protocol: ShimProtocol, platform: Platform) -> Option<&EmbeddedShim> {
        self.shims
            .iter()
            .find(|shim| shim.protocol == protocol && shim.platform == platform)
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
        EmbeddedShimCatalog::from_embedded_assets(
            names.iter().map(|name| (*name, Cow::Borrowed(&b"abc"[..]))),
        )
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
        assert!(aliased.find(ShimProtocol::Ssh, platform).is_some());
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
