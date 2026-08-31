use std::borrow::Cow;

#[cfg(feature = "embed-shims")]
use rust_embed::RustEmbed;
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

#[cfg(feature = "embed-shims")]
#[derive(RustEmbed)]
#[folder = "dist/shims/"]
#[include = "skyhook-shim-*"]
struct ShimAssets;

#[derive(Clone, Copy, Debug, Default)]
pub struct EmbeddedShimCatalog;

impl EmbeddedShimCatalog {
    pub fn all(self) -> Result<Vec<EmbeddedShim>, ArtifactError> {
        let mut shims = embedded_names()
            .into_iter()
            .map(|name| parse_artifact(&name))
            .collect::<Result<Vec<_>, _>>()?;
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
        Ok(shims)
    }

    pub fn find(self, arch: &str, os: &str) -> Result<Option<EmbeddedShim>, ArtifactError> {
        let arch = normalize_arch(arch);
        let os = normalize_os(os);
        Ok(self
            .all()?
            .into_iter()
            .find(|shim| shim.arch == arch && shim.os == os))
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

fn parse_artifact(name: &str) -> Result<EmbeddedShim, ArtifactError> {
    let (arch, os, extension) = parse_name(name)?;
    let bytes = embedded_bytes(name).ok_or_else(|| ArtifactError::Missing(name.to_owned()))?;
    Ok(EmbeddedShim {
        arch,
        os,
        extension,
        file_name: name.to_owned(),
        bytes,
    })
}

#[cfg(feature = "embed-shims")]
fn embedded_names() -> Vec<String> {
    ShimAssets::iter().map(Cow::into_owned).collect()
}

#[cfg(not(feature = "embed-shims"))]
fn embedded_names() -> Vec<String> {
    Vec::new()
}

#[cfg(feature = "embed-shims")]
fn embedded_bytes(name: &str) -> Option<Cow<'static, [u8]>> {
    ShimAssets::get(name).map(|file| file.data)
}

#[cfg(not(feature = "embed-shims"))]
fn embedded_bytes(_name: &str) -> Option<Cow<'static, [u8]>> {
    None
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
        let catalog = EmbeddedShimCatalog;
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
}
