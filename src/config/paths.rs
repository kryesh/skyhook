use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Skyhook's directory inside a workspace.
const WORKSPACE_DIRECTORY: &str = ".skyhook";

/// A workspace's session history when no session root is configured.
pub fn workspace_session_root(workspace: &Path) -> PathBuf {
    workspace.join(WORKSPACE_DIRECTORY).join("sessions")
}

pub(super) fn workspace_config_path(workspace: &Path) -> PathBuf {
    workspace.join(WORKSPACE_DIRECTORY).join("config.yaml")
}

/// Skyhook's user configuration directories, most preferred first:
/// `$XDG_CONFIG_HOME/skyhook`, then `$HOME/.config/skyhook`.
pub(crate) fn user_config_directories() -> Vec<PathBuf> {
    config_directories(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

pub(crate) fn user_config_directory() -> Option<PathBuf> {
    user_config_directories().into_iter().next()
}

pub(super) fn user_config_paths() -> Vec<PathBuf> {
    user_config_directories()
        .into_iter()
        .map(|directory| directory.join("config.yaml"))
        .collect()
}

fn config_directories(xdg: Option<OsString>, home: Option<OsString>) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(root) = xdg.filter(|root| !root.is_empty()) {
        directories.push(PathBuf::from(root).join("skyhook"));
    }
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        let fallback = PathBuf::from(home).join(".config/skyhook");
        if !directories.contains(&fallback) {
            directories.push(fallback);
        }
    }
    directories
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_empty_roots_and_deduplicates_directories() {
        assert!(config_directories(None, None).is_empty());
        assert!(config_directories(Some("".into()), Some("".into())).is_empty());
        assert_eq!(
            config_directories(Some("".into()), Some("/home/me".into())),
            [PathBuf::from("/home/me/.config/skyhook")]
        );
        assert_eq!(
            config_directories(Some("/home/me/.config".into()), Some("/home/me".into())),
            [PathBuf::from("/home/me/.config/skyhook")]
        );
        assert_eq!(
            config_directories(Some("/xdg".into()), Some("/home/me".into())),
            [
                PathBuf::from("/xdg/skyhook"),
                PathBuf::from("/home/me/.config/skyhook")
            ]
        );
    }
}
