//! A Windows origin's agent: the one it inherited, else the OpenSSH agent service's.
use crate::origin::{ProcessEnvironment, error::TransportError, ssh::SshRoute};

/// Where the OpenSSH for Windows agent service listens.
const SERVICE_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

/// The OpenSSH for Windows agent is a system service that keeps every key added to it
/// in the user's registry, so a Windows origin starts no agent of its own and offers
/// only the user's.
#[derive(Default)]
pub struct Authentication;

impl Authentication {
    /// Environment for an SSH process started on this machine, which needs nothing
    /// to reach the user's agent.
    pub async fn route_environment(
        &self,
        _: &SshRoute,
    ) -> Result<ProcessEnvironment, TransportError> {
        Ok(ProcessEnvironment::new())
    }
}

/// The agent this process inherited in SSH_AUTH_SOCK, else the agent service while it
/// runs.
pub(in crate::origin::ssh) fn user_agent() -> Option<String> {
    super::inherited().or_else(|| service_running().then(|| SERVICE_PIPE.to_owned()))
}

/// Whether the agent service's pipe exists. Listing it, unlike opening it, does not
/// connect to the agent.
fn service_running() -> bool {
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW},
    };
    let name: Vec<u16> = SERVICE_PIPE.encode_utf16().chain([0]).collect();
    // SAFETY: the structure is plain data, valid when zeroed.
    let mut found: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    // SAFETY: the name is NUL-terminated and `found` is a WIN32_FIND_DATAW.
    let search = unsafe { FindFirstFileW(name.as_ptr(), &raw mut found) };
    if search == INVALID_HANDLE_VALUE {
        return false;
    }
    // SAFETY: the search handle was just opened and is closed once.
    unsafe { FindClose(search) };
    true
}
