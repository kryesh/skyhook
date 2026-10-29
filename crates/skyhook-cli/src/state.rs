use serde::{Deserialize, Deserializer, Serialize};
use skyhook::{
    fs::{CommitMode, PermissionPolicy, StagedFile},
    provider::profile::ModelRef,
    tool::policy::ModeName,
};
use std::path::{Path, PathBuf};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SavedState {
    #[serde(deserialize_with = "lenient")]
    pub model: Option<ModelRef>,
    #[serde(deserialize_with = "lenient")]
    pub mode: Option<ModeName>,
    pub sidebar: bool,
}
impl Default for SavedState {
    fn default() -> Self {
        Self {
            model: None,
            mode: None,
            sidebar: true,
        }
    }
}
/// A saved name that no longer parses is dropped, for the default to apply,
/// without discarding the other settings.
fn lenient<'de, D: Deserializer<'de>, T: std::str::FromStr>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.and_then(|name| name.parse().ok()))
}

/// Product UI preferences are independent of the shared session directory.
pub fn state_path(workspace: &Path, identity: &super::Identity) -> PathBuf {
    skyhook::config::workspace_directory(workspace).join(identity.state_file)
}
pub fn load(path: &Path) -> (SavedState, Option<String>) {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(state) => (state, None),
            Err(error) => (
                SavedState::default(),
                Some(format!("Could not read UI state: {error}")),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (SavedState::default(), None),
        Err(e) => (
            SavedState::default(),
            Some(format!("Could not read UI state: {e}")),
        ),
    }
}
/// Change saved settings without disturbing the others.
pub fn update(path: &Path, change: impl FnOnce(&mut SavedState)) -> std::io::Result<()> {
    // Concurrent savers must not overwrite each other's setting.
    static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _saving = SAVING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut state, _) = load(path);
    change(&mut state);
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let mut staged = StagedFile::create(path, PermissionPolicy::Private)?;
    staged.write(&serde_json::to_vec(&state)?)?;
    Ok(staged.commit(CommitMode::Replace)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_keep_other_settings_and_the_sidebar_defaults_on() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        assert!(load(&path).0.sidebar);
        let model: ModelRef = "p/m".parse().unwrap();
        update(&path, |state| state.model = Some(model.clone())).unwrap();
        update(&path, |state| state.mode = Some("look".parse().unwrap())).unwrap();
        update(&path, |state| state.sidebar = false).unwrap();
        let (state, warning) = load(&path);
        assert_eq!(
            (
                state.model.as_ref(),
                state.mode.as_ref().map(ModeName::as_str),
                state.sidebar,
                warning
            ),
            (Some(&model), Some("look"), false, None)
        );
        // Invalid saved names are dropped; the other settings still load.
        let saved = br#"{"model":"m","mode":" ","sidebar":false}"#;
        std::fs::write(&path, saved).unwrap();
        let (state, warning) = load(&path);
        assert!(state.model.is_none() && !state.sidebar && state.mode.is_none());
        assert!(warning.is_none());
    }
}
