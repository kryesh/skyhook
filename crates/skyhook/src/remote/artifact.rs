//! Embedded remote shims, one per platform, named `<os>-<arch>` such as `linux-x86_64`.
use std::{
    borrow::Cow,
    sync::{Arc, OnceLock},
};

use thiserror::Error;

use crate::remote::error::DeploymentError;
use skyhook_worker::target::{Os, Platform, ShimName};

/// A shim as the catalog read it: its bytes, and the file they install as.
#[derive(Clone, Debug)]
pub struct EmbeddedShim {
    pub platform: Platform,
    pub bytes: Cow<'static, [u8]>,
    installed: ShimName,
}

/// Reads a named shim's bytes. The catalog reads a shim when a connection first needs
/// its name or its bytes, and keeps one read of it.
pub type ShimLoader = fn(&str) -> Option<Cow<'static, [u8]>>;

#[derive(Debug)]
struct ShimEntry {
    platform: Platform,
    name: Box<str>,
    /// The one read of the shim that is kept, so the name connections look for is the one its
    /// bytes install as, whatever the loader reads later.
    read: OnceLock<EmbeddedShim>,
}

impl ShimEntry {
    fn read(&self, load: ShimLoader) -> Option<&EmbeddedShim> {
        if let Some(shim) = self.read.get() {
            return Some(shim);
        }
        let (platform, bytes) = (self.platform, load(&self.name)?);
        let installed = ShimName::of(&bytes, platform.os);
        Some(self.read.get_or_init(|| EmbeddedShim {
            platform,
            bytes,
            installed,
        }))
    }
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
    /// Builds a catalog from `<os>-<arch>` artifact names that `load` reads.
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
                let [os, arch] = name.split('-').collect::<Vec<_>>()[..] else {
                    return Err(invalid());
                };
                Ok(ShimEntry {
                    platform: Platform::parse(os, arch).ok_or_else(invalid)?,
                    name: name.into(),
                    read: OnceLock::new(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            shims: shims.into(),
            load,
        })
    }

    /// The names the shims built for `os` that can be read install as.
    pub(crate) fn installed(&self, os: Os) -> Vec<ShimName> {
        (self.shims.iter())
            .filter(|shim| shim.platform.os == os)
            .filter_map(|shim| Some(shim.read(self.load)?.installed_name()))
            .collect()
    }

    pub(crate) fn find(&self, platform: Platform) -> Result<&EmbeddedShim, DeploymentError> {
        if self.shims.is_empty() {
            return Err(DeploymentError::NoShims);
        }
        let entry = self
            .shims
            .iter()
            .find(|shim| shim.platform == platform)
            .ok_or(DeploymentError::NoShim { platform })?;
        entry
            .read(self.load)
            .ok_or_else(|| DeploymentError::Unreadable {
                name: entry.name.clone(),
            })
    }
}

impl EmbeddedShim {
    /// The file the shim installs as on its platform.
    pub fn installed_name(&self) -> ShimName {
        self.installed.clone()
    }
}

#[derive(Clone, Debug, Error)]
pub enum ArtifactError {
    #[error("invalid shim artifact name `{0}`; expected <os>-<arch> in lowercase")]
    InvalidName(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    fn catalog(names: &[&str]) -> Result<EmbeddedShimCatalog, ArtifactError> {
        EmbeddedShimCatalog::from_embedded_assets(names, |_| Some(Cow::Borrowed(b"abc")))
    }

    #[test]
    fn finds_shims_by_platform_and_arch_aliases() {
        let names = [
            "linux-x86_64",
            "linux-aarch64",
            "linux-x86_64.d",
            "windows-x86_64",
        ];
        let shims = catalog(&names).unwrap();
        let platform = Platform::parse("linux", "x86_64").unwrap();
        let shim = shims.find(platform).unwrap();
        assert_eq!((shim.platform, &*shim.bytes), (platform, &b"abc"[..]));
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(shim.installed_name().digest().to_string(), sha);
        // A connection lists the installed names of its own OS's shims.
        assert_eq!(shims.installed(Os::Linux), vec![shim.installed_name(); 2]);
        // An alias-named artifact serves the canonical platform.
        let aliased = catalog(&["linux-amd64"]).unwrap();
        assert!(aliased.find(platform).is_ok());
        // A shim staged again since a connection installed it is still the one read
        // first: later connections look for the name that one installed as.
        static READS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        let restaged = EmbeddedShimCatalog::from_embedded_assets(["linux-x86_64"], |_| {
            let read = READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(Cow::Owned(vec![read]))
        });
        let restaged = restaged.unwrap();
        let installed = restaged.find(platform).unwrap().installed_name();
        assert_eq!(restaged.installed(Os::Linux), [installed]);
    }

    #[test]
    fn find_distinguishes_missing_from_unreadable_shims() {
        let platform = Platform::parse("linux", "x86_64").unwrap();
        let find = |catalog: EmbeddedShimCatalog| catalog.find(platform).map(drop);
        assert_matches!(
            find(EmbeddedShimCatalog::default()),
            Err(DeploymentError::NoShims)
        );
        assert_matches!(
            find(catalog(&["linux-aarch64"]).unwrap()),
            Err(DeploymentError::NoShim { .. })
        );
        let unreadable =
            EmbeddedShimCatalog::from_embedded_assets(["linux-x86_64"], |_| None).unwrap();
        assert_matches!(
            find(unreadable),
            Err(DeploymentError::Unreadable { name }) if &*name == "linux-x86_64"
        );
    }

    #[test]
    fn rejects_invalid_names() {
        for name in [
            "",
            "linux-x86_64-extra",
            "linux-ssh-x86_64",
            "linux-",
            "Linux-x86_64",
            "../linux-x86_64",
            "linux-$(uname)",
            "linux-riscv64",
            "darwin-x86_64",
        ] {
            assert_matches!(
                catalog(&[name]),
                Err(ArtifactError::InvalidName(value)) if value == name,
                "accepted invalid name {name:?}"
            );
        }
    }
}
