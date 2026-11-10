//! This machine as an origin: the SSH and WinRM connections it opens to further
//! targets, each a byte stream a shim's protocol runs over, and why they fail.

pub mod error;
pub mod ssh;
pub mod transport;
pub mod winrm;

/// How an origin reaches a connection's destination.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub enum Leg {
    Ssh(ssh::SshRoute),
    Winrm(winrm::WinrmRoute),
}

impl Leg {
    /// How much longer a worker that relayed this leg may take to end its connection:
    /// an SSH process is killed as its call stops, while a WinRM shell's deletion is
    /// awaited, within its bound, what was left of its creation included.
    pub fn ending(&self) -> std::time::Duration {
        match self {
            Self::Ssh(_) => std::time::Duration::ZERO,
            Self::Winrm(_) => winrm::DELETE_BOUND,
        }
    }
}

/// Authentication variables added to processes started on a credential origin.
pub type ProcessEnvironment = std::collections::BTreeMap<String, String>;
