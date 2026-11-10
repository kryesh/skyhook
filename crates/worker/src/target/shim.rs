//! The file a shim installs as: its SHA-256, with its platform's executable suffix.
use std::fmt;

use skyhook_base::hex::Sha256;

use super::Os;

/// What the name of a shim's upload starts with, beside the installed shims, until it
/// is checked and installed.
pub const UPLOAD_PREFIX: &str = ".upload-";

/// The name a shim built for `os` installs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShimName {
    digest: Sha256,
    os: Os,
}

impl ShimName {
    /// The name of the shim built for `os` whose contents are `contents`.
    pub fn of(contents: &[u8], os: Os) -> Self {
        let digest = Sha256::of(contents);
        Self { digest, os }
    }

    /// `name`, when it names a shim built for `os`.
    pub fn parse(name: &str, os: Os) -> Option<Self> {
        let digest = Sha256::from_lowercase_hex(name.strip_suffix(os.executable_suffix())?)?;
        Some(Self { digest, os })
    }

    /// The SHA-256 of the shim's contents.
    pub const fn digest(&self) -> &Sha256 {
        &self.digest
    }
}

impl fmt::Display for ShimName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", self.digest, self.os.executable_suffix())
    }
}
