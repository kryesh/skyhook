//! Skyhook is a provider-neutral coding-agent harness whose registered tools are
//! available both as model tool calls and inside a sandboxed JavaScript runtime.

pub mod agent;
pub mod config;
pub mod execution;
pub mod identity;
pub mod job;
pub mod media;
pub mod provider;
pub mod remote;
pub mod session;
pub mod target;
pub mod tool;

pub(crate) fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    use digest::Digest as _;
    use std::fmt::Write as _;

    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        })
}
