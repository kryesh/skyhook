//! The SSH agents an origin offers its routes. Every origin offers the user's own agent
//! when it has one, and never adds keys to it. Only a unix origin without one can
//! offer a private agent it starts itself; a Windows origin starts none, so there a
//! route that needs an agent and finds none fails as it would with no agent at all.
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::Authentication;
#[cfg(unix)]
pub(super) use unix::user_agent;
#[cfg(windows)]
pub use windows::Authentication;
#[cfg(windows)]
pub(super) use windows::user_agent;

pub(super) const AUTH_SOCK: &str = "SSH_AUTH_SOCK";

/// The agent socket this process inherited.
fn inherited() -> Option<String> {
    std::env::var(AUTH_SOCK)
        .ok()
        .filter(|socket| !socket.is_empty())
}
