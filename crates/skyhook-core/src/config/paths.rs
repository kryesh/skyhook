use std::path::PathBuf;

pub(crate) fn user_config_directory() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME").filter(|root| !root.is_empty()) {
        return Some(PathBuf::from(root).join("skyhook"));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config/skyhook"))
}
