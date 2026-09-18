use std::{ffi::OsString, path::PathBuf};

pub(crate) fn user_config_directory() -> Option<PathBuf> {
    user_config_paths()
        .into_iter()
        .next()
        .and_then(|path| path.parent().map(PathBuf::from))
}

pub(super) fn user_config_paths() -> Vec<PathBuf> {
    candidate_paths(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn candidate_paths(xdg: Option<OsString>, home: Option<OsString>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(root) = xdg.filter(|root| !root.is_empty()) {
        paths.push(PathBuf::from(root).join("skyhook/config.toml"));
    }
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        let path = PathBuf::from(home).join(".config/skyhook/config.toml");
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_empty_roots_and_deduplicates_candidates() {
        assert!(candidate_paths(None, None).is_empty());
        assert!(candidate_paths(Some("".into()), Some("".into())).is_empty());
        assert_eq!(
            candidate_paths(Some("".into()), Some("/home/me".into())),
            [PathBuf::from("/home/me/.config/skyhook/config.toml")]
        );
        assert_eq!(
            candidate_paths(Some("/home/me/.config".into()), Some("/home/me".into())),
            [PathBuf::from("/home/me/.config/skyhook/config.toml")]
        );
        assert_eq!(
            candidate_paths(Some("/xdg".into()), Some("/home/me".into())),
            [
                PathBuf::from("/xdg/skyhook/config.toml"),
                PathBuf::from("/home/me/.config/skyhook/config.toml")
            ]
        );
    }
}
