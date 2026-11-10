//! Fixed-width bytes as hexadecimal text.

use std::fmt;

/// `N` bytes, such as a digest or a random identifier, written as lowercase hexadecimal.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest<const N: usize>([u8; N]);

/// A SHA-256 digest.
pub type Sha256 = Digest<32>;

impl Sha256 {
    pub fn of(bytes: impl AsRef<[u8]>) -> Self {
        use sha2::Digest as _;
        Self(sha2::Sha256::digest(bytes).into())
    }
}

impl<const N: usize> Digest<N> {
    pub const fn from_bytes(bytes: [u8; N]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; N] {
        &self.0
    }

    /// Exactly `N` bytes of hexadecimal in either case.
    pub fn from_hex(text: &str) -> Option<Self> {
        // Radix parsing alone would accept a leading `+`.
        if text.len() != N * 2 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0; N];
        for (slot, pair) in bytes.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
            *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        Some(Self(bytes))
    }

    /// `text` when it is spelled as a digest displays: lowercase.
    pub fn from_lowercase_hex(text: &str) -> Option<Self> {
        match text.bytes().any(|byte| byte.is_ascii_uppercase()) {
            true => None,
            false => Self::from_hex(text),
        }
    }
}

impl<const N: usize> fmt::Display for Digest<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        (self.0.iter()).try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}

impl<const N: usize> fmt::Debug for Digest<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}
