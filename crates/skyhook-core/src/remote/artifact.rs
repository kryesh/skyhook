use std::{borrow::Cow, sync::Arc};

use thiserror::Error;

#[derive(Clone, Debug)]
pub struct EmbeddedShim {
    pub arch: String,
    /// Canonical platform name, such as `linux`, `macos`, or `windows`.
    pub os: String,
    pub protocol: String,
    /// Binary name derived from the canonical platform and protocol.
    pub binary: String,
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
        Self::from_embedded_assets(
            assets
                .iter()
                .map(|(name, bytes)| (*name, Cow::Borrowed(*bytes))),
        )
    }

    /// Builds a validated catalog from artifact names and borrowed or owned bytes.
    ///
    /// Names follow `<platform>-<protocol>-<arch>[.<extension>]`. The three
    /// components use lowercase ASCII letters, digits, or underscores, never
    /// hyphens. Common platform and architecture aliases are canonicalized,
    /// including in the installed binary name. Names need not be static and
    /// payloads can be owned, so embedding libraries need no core dependency.
    pub fn from_embedded_assets<I, N>(assets: I) -> Result<Self, ArtifactError>
    where
        I: IntoIterator<Item = (N, Cow<'static, [u8]>)>,
        N: AsRef<str>,
    {
        let mut shims = assets
            .into_iter()
            .map(|(name, bytes)| {
                let name = name.as_ref();
                let ParsedName {
                    arch,
                    os,
                    protocol,
                    extension,
                } = parse_name(name)?;
                Ok(EmbeddedShim {
                    binary: format!("{os}-{protocol}"),
                    arch,
                    os,
                    protocol,
                    extension,
                    file_name: name.to_owned(),
                    bytes,
                })
            })
            .collect::<Result<Vec<_>, ArtifactError>>()?;
        shims.sort_by(|left, right| {
            (&left.protocol, &left.os, &left.arch, &left.file_name).cmp(&(
                &right.protocol,
                &right.os,
                &right.arch,
                &right.file_name,
            ))
        });
        for pair in shims.windows(2) {
            if pair[0].protocol == pair[1].protocol
                && pair[0].arch == pair[1].arch
                && pair[0].os == pair[1].os
            {
                return Err(ArtifactError::DuplicatePlatform {
                    protocol: pair[0].protocol.clone(),
                    arch: pair[0].arch.clone(),
                    os: pair[0].os.clone(),
                });
            }
        }
        Ok(Self {
            shims: shims.into(),
        })
    }

    /// Reports whether this build supplied any remote artifacts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shims.is_empty()
    }

    /// Finds an artifact after trimming and case-folding all components and
    /// normalizing common architecture and OS aliases.
    pub fn find(&self, protocol: &str, arch: &str, os: &str) -> Option<EmbeddedShim> {
        let protocol = normalize_component(protocol);
        let arch = normalize_arch(arch);
        let os = normalize_os(os);
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
        self.extension.as_deref().map_or_else(
            || self.binary.clone(),
            |extension| format!("{}.{extension}", self.binary),
        )
    }
}

#[derive(Debug)]
struct ParsedName {
    arch: String,
    os: String,
    protocol: String,
    extension: Option<String>,
}

fn parse_name(name: &str) -> Result<ParsedName, ArtifactError> {
    let invalid = || ArtifactError::InvalidName(name.to_owned());
    let (stem, extension) = name
        .split_once('.')
        .map_or((name, None), |(stem, extension)| (stem, Some(extension)));
    let mut components = stem.split('-');
    let os = components.next().ok_or_else(invalid)?;
    let protocol = components.next().ok_or_else(invalid)?;
    let arch = components.next().ok_or_else(invalid)?;
    if components.next().is_some()
        || ![os, protocol, arch].into_iter().all(valid_component)
        || extension.is_some_and(|value| !valid_component(value))
    {
        return Err(invalid());
    }
    Ok(ParsedName {
        arch: normalize_arch(arch),
        os: normalize_os(os),
        protocol: protocol.to_owned(),
        extension: extension.map(str::to_owned),
    })
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn normalize_component(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalize_arch(value: &str) -> String {
    let value = normalize_component(value);
    match value.as_str() {
        "amd64" | "x64" => "x86_64".to_owned(),
        "arm64" => "aarch64".to_owned(),
        _ => value,
    }
}

fn normalize_os(value: &str) -> String {
    let value = normalize_component(value);
    match value.as_str() {
        "gnu/linux" => "linux".to_owned(),
        "darwin" => "macos".to_owned(),
        "windows_nt" => "windows".to_owned(),
        _ => value,
    }
}

#[derive(Clone, Debug, Error)]
pub enum ArtifactError {
    #[error(
        "invalid shim artifact name `{0}`; expected <platform>-<protocol>-<arch>[.<extension>] with lowercase ASCII letters, digits, or underscores in each component"
    )]
    InvalidName(String),
    #[error("multiple embedded shims target {os}-{protocol}-{arch}")]
    DuplicatePlatform {
        protocol: String,
        arch: String,
        os: String,
    },
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
    fn parses_artifact_metadata_and_installed_names() {
        let catalog = catalog(&[
            "linux-ssh-x86_64",
            "linux-ssh-aarch64",
            "windows-winrm-x86_64.exe",
        ])
        .unwrap();
        let linux = catalog.find("ssh", "x86_64", "linux").unwrap();
        assert_eq!(linux.installed_name(), "linux-ssh");
        assert_eq!(
            linux.sha256(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let windows = catalog.find("winrm", "x86_64", "windows").unwrap();
        assert_eq!(windows.installed_name(), "windows-winrm.exe");
    }

    #[test]
    fn normalizes_lookup_aliases_and_case() {
        let catalog = catalog(&[
            "linux-ssh-x86_64",
            "macos-ssh-aarch64",
            "windows-winrm-x86_64.exe",
        ])
        .unwrap();
        for arch in ["x86_64", "amd64", "x64", " AMD64 "] {
            assert!(catalog.find(" SSH ", arch, " GNU/Linux ").is_some());
            assert!(catalog.find("WinRM", arch, "Windows_NT").is_some());
        }
        for arch in ["aarch64", "arm64", " ARM64 "] {
            assert!(catalog.find("ssh", arch, " Darwin ").is_some());
            assert!(catalog.find("ssh", arch, "MacOS").is_some());
        }
    }

    #[test]
    fn canonicalizes_asset_aliases() {
        let catalog = catalog(&["darwin-ssh-arm64", "windows_nt-winrm-amd64.exe"]).unwrap();
        let macos = catalog.find("ssh", "aarch64", "macos").unwrap();
        assert_eq!(macos.file_name, "darwin-ssh-arm64");
        assert_eq!(macos.installed_name(), "macos-ssh");
        let windows = catalog.find("winrm", "x64", "windows").unwrap();
        assert_eq!(windows.installed_name(), "windows-winrm.exe");
    }

    #[test]
    fn separates_protocols_platforms_and_architectures() {
        let catalog = catalog(&[
            "linux-ssh-x86_64",
            "linux-winrm-x86_64",
            "windows-winrm-x86_64.exe",
            "linux-ssh-aarch64",
            "future_os-other_protocol-riscv64",
        ])
        .unwrap();
        assert_eq!(
            catalog.find("winrm", "x64", "linux").unwrap().binary,
            "linux-winrm"
        );
        assert!(catalog.find("ssh", "x64", "windows").is_none());
        assert!(catalog.find("winrm", "arm64", "linux").is_none());
        assert!(catalog.find("other", "x64", "linux").is_none());
        assert!(catalog.find("ssh", "riscv64", "linux").is_none());
        assert!(
            catalog
                .find("other_protocol", "riscv64", "future_os")
                .is_some()
        );
    }

    #[test]
    fn rejects_duplicate_canonical_targets_regardless_of_extension_or_order() {
        for names in [
            ["linux-ssh-x86_64", "linux-ssh-x86_64"],
            ["linux-ssh-x86_64.exe", "linux-ssh-x86_64"],
            ["linux-ssh-amd64", "linux-ssh-x64"],
            ["macos-ssh-aarch64", "darwin-ssh-arm64"],
            ["windows-winrm-x86_64.exe", "windows_nt-winrm-amd64"],
        ] {
            for pair in [names, [names[1], names[0]]] {
                let error = catalog(&[pair[0], "linux-other-riscv64", pair[1]]).unwrap_err();
                let ArtifactError::DuplicatePlatform { protocol, arch, os } = error else {
                    panic!("expected duplicate target for {pair:?}");
                };
                assert!(matches!(protocol.as_str(), "ssh" | "winrm"));
                assert!(matches!(arch.as_str(), "x86_64" | "aarch64"));
                assert!(matches!(os.as_str(), "linux" | "macos" | "windows"));
            }
        }
    }

    #[test]
    fn rejects_invalid_names() {
        for name in [
            "",
            "linux-ssh-x86_64-extra",
            "skyhook-shim-x86_64-linux",
            "-ssh-x86_64",
            "linux--x86_64",
            "linux-ssh-",
            "Linux-ssh-x86_64",
            " linux-ssh-x86_64",
            "linux-ssh-x86_64\n",
            "../linux-ssh-x86_64",
            "/linux-ssh-x86_64",
            "dir\\linux-ssh-x86_64",
            "linux-ss h-x86_64",
            "linux-ssh-x86_64;echo",
            "linux-ssh-$(uname)",
            "línux-ssh-x86_64",
            "linux-ssh-x86_64.",
            "linux-ssh-x86_64.exe.bak",
            "linux-ssh-x86_64.ex-e",
            "linux-ssh-x86_64.EXE",
        ] {
            assert!(
                matches!(catalog(&[name]), Err(ArtifactError::InvalidName(value)) if value == name),
                "accepted invalid name {name:?}"
            );
        }
    }
}
