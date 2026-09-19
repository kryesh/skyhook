use std::{borrow::Cow, sync::Arc};

use thiserror::Error;

/// A remote shim named `<os>-<protocol>-<arch>`, such as `linux-ssh-x86_64`.
#[derive(Clone, Debug)]
pub struct EmbeddedShim {
    pub arch: String,
    pub os: String,
    pub protocol: String,
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
                let parts = name.split('-').collect::<Vec<_>>();
                let valid = |part: &&str| {
                    !part.is_empty()
                        && part
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                };
                let [os, protocol, arch] = parts[..] else {
                    return Err(ArtifactError::InvalidName(name.to_owned()));
                };
                if !parts.iter().all(valid) {
                    return Err(ArtifactError::InvalidName(name.to_owned()));
                }
                Ok(EmbeddedShim {
                    arch: normalize_arch(arch),
                    os: os.to_owned(),
                    protocol: protocol.to_owned(),
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

    /// Finds the shim for a protocol and the `uname -m` / `uname -s` values of a host.
    pub fn find(&self, protocol: &str, arch: &str, os: &str) -> Option<EmbeddedShim> {
        let arch = normalize_arch(arch);
        let os = os.trim().to_ascii_lowercase();
        self.shims
            .iter()
            .find(|shim| shim.protocol == protocol && shim.arch == arch && shim.os == os)
            .cloned()
    }
}

impl EmbeddedShim {
    #[must_use]
    pub fn sha256(&self) -> String {
        crate::sha256_hex(&self.bytes)
    }

    #[must_use]
    pub fn installed_name(&self) -> String {
        format!("{}-{}", self.os, self.protocol)
    }
}

fn normalize_arch(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "amd64" => "x86_64".to_owned(),
        "arm64" => "aarch64".to_owned(),
        other => other.to_owned(),
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
    fn finds_shims_by_uname_values_and_arch_aliases() {
        let catalog = catalog(&[
            "linux-ssh-x86_64",
            "linux-ssh-aarch64",
            "linux-ssh-x86_64.d",
        ])
        .unwrap();
        let shim = catalog.find("ssh", "x86_64\n", "Linux").unwrap();
        assert_eq!((shim.arch.as_str(), &*shim.bytes), ("x86_64", &b"abc"[..]));
        assert_eq!(shim.installed_name(), "linux-ssh");
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(shim.sha256(), sha);
        assert_eq!(
            catalog.find("ssh", "arm64", "linux").unwrap().arch,
            "aarch64"
        );
        assert_eq!(
            catalog.find("ssh", "amd64", "linux").unwrap().arch,
            "x86_64"
        );
        assert!(catalog.find("ssh", "riscv64", "linux").is_none());
        assert!(catalog.find("ssh", "x86_64", "darwin").is_none());
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
        ] {
            assert!(
                matches!(catalog(&[name]), Err(ArtifactError::InvalidName(value)) if value == name),
                "accepted invalid name {name:?}"
            );
        }
    }
}
