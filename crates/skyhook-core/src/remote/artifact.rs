use std::{borrow::Cow, sync::Arc};

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct EmbeddedShim {
    pub arch: String,
    pub os: String,
    pub extension: Option<String>,
    pub file_name: String,
    pub bytes: Cow<'static, [u8]>,
}

#[derive(Clone, Debug, Default)]
pub struct EmbeddedShimCatalog {
    shims: Arc<[EmbeddedShim]>,
}

impl EmbeddedShimCatalog {
    /// Builds a validated catalog from compile-time artifact names and bytes.
    pub fn from_assets(
        assets: &'static [(&'static str, &'static [u8])],
    ) -> Result<Self, ArtifactError> {
        let mut shims = assets
            .iter()
            .map(|(name, bytes)| {
                let (arch, os, extension) = parse_name(name)?;
                Ok(EmbeddedShim {
                    arch,
                    os,
                    extension,
                    file_name: (*name).to_owned(),
                    bytes: Cow::Borrowed(*bytes),
                })
            })
            .collect::<Result<Vec<_>, ArtifactError>>()?;
        shims.sort_by(|left, right| {
            (&left.arch, &left.os, &left.file_name).cmp(&(&right.arch, &right.os, &right.file_name))
        });
        for pair in shims.windows(2) {
            if pair[0].arch == pair[1].arch && pair[0].os == pair[1].os {
                return Err(ArtifactError::DuplicatePlatform {
                    arch: pair[0].arch.clone(),
                    os: pair[0].os.clone(),
                });
            }
        }
        Ok(Self {
            shims: shims.into(),
        })
    }

    /// Returns every artifact in canonical platform order.
    pub fn all(&self) -> Result<Vec<EmbeddedShim>, ArtifactError> {
        Ok(self.shims.to_vec())
    }

    /// Reports whether this build supplied any remote artifacts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shims.is_empty()
    }

    /// Finds an artifact after normalizing common architecture and OS aliases.
    pub fn find(&self, arch: &str, os: &str) -> Result<Option<EmbeddedShim>, ArtifactError> {
        let arch = normalize_arch(arch);
        let os = normalize_os(os);
        Ok(self
            .shims
            .iter()
            .find(|shim| shim.arch == arch && shim.os == os)
            .cloned())
    }
}

impl EmbeddedShim {
    #[must_use]
    pub fn sha256(self) -> String {
        format!("{:x}", Sha256::digest(&self.bytes))
    }

    #[must_use]
    pub fn installed_name(self) -> String {
        self.extension.as_deref().map_or_else(
            || "skyhook-shim".to_owned(),
            |extension| format!("skyhook-shim.{extension}"),
        )
    }
}

fn parse_name(name: &str) -> Result<(String, String, Option<String>), ArtifactError> {
    let platform = name
        .strip_prefix("skyhook-shim-")
        .ok_or_else(|| ArtifactError::InvalidName(name.to_owned()))?;
    let (platform, extension) = platform
        .split_once('.')
        .map_or((platform, None), |(platform, extension)| {
            (platform, Some(extension))
        });
    let (arch, os) = platform
        .rsplit_once('-')
        .ok_or_else(|| ArtifactError::InvalidName(name.to_owned()))?;
    let valid = !arch.is_empty()
        && !os.is_empty()
        && arch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && os
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && extension.is_none_or(|value| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        });
    if !valid {
        return Err(ArtifactError::InvalidName(name.to_owned()));
    }
    Ok((arch.to_owned(), os.to_owned(), extension.map(str::to_owned)))
}

fn normalize_arch(value: &str) -> &str {
    match value.trim() {
        "amd64" | "x64" => "x86_64",
        "arm64" => "aarch64",
        value => value,
    }
}

fn normalize_os(value: &str) -> &str {
    match value.trim().to_ascii_lowercase().as_str() {
        "linux" | "gnu/linux" => "linux",
        "darwin" | "macos" => "macos",
        "windows" | "windows_nt" => "windows",
        _ => value.trim(),
    }
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("invalid shim artifact name `{0}`; expected skyhook-shim-<arch>-<os>[.<extension>]")]
    InvalidName(String),
    #[error("embedded shim `{0}` disappeared from the artifact catalog")]
    Missing(String),
    #[error("multiple embedded shims target {arch}-{os}")]
    DuplicatePlatform { arch: String, os: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_catalog_is_valid_and_normalizes_platform_names() {
        let catalog = EmbeddedShimCatalog::default();
        let _ = catalog.all().unwrap();
        assert_eq!(normalize_arch("amd64"), "x86_64");
        assert_eq!(normalize_arch("arm64"), "aarch64");
        assert_eq!(normalize_os("Darwin"), "macos");
    }

    #[test]
    fn compact_names_preserve_optional_extensions() {
        assert_eq!(
            parse_name("skyhook-shim-x86_64-linux").unwrap(),
            ("x86_64".to_owned(), "linux".to_owned(), None)
        );
        assert_eq!(
            parse_name("skyhook-shim-x86_64-windows.exe").unwrap(),
            (
                "x86_64".to_owned(),
                "windows".to_owned(),
                Some("exe".to_owned())
            )
        );
    }

    #[test]
    fn assets_are_sorted_and_duplicates_are_rejected() {
        static ASSETS: &[(&str, &[u8])] = &[
            ("skyhook-shim-x86_64-linux", b"x86"),
            ("skyhook-shim-aarch64-linux", b"arm"),
        ];
        let catalog = EmbeddedShimCatalog::from_assets(ASSETS).unwrap();
        assert_eq!(catalog.all().unwrap()[0].arch, "aarch64");
        assert_eq!(
            catalog.find("amd64", "Linux").unwrap().unwrap().bytes,
            b"x86".as_slice()
        );

        static DUPLICATES: &[(&str, &[u8])] = &[
            ("skyhook-shim-x86_64-linux", b"one"),
            ("skyhook-shim-x86_64-linux.exe", b"two"),
        ];
        assert!(matches!(
            EmbeddedShimCatalog::from_assets(DUPLICATES),
            Err(ArtifactError::DuplicatePlatform { .. })
        ));
    }
}
