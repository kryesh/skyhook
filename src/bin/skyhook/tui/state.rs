use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SavedState {
    pub model: Option<String>,
}
/// UI state lives with the workspace's sessions.
pub fn state_path(workspace: &Path) -> PathBuf {
    workspace.join(".skyhook/state.json")
}
pub fn load(workspace: &Path) -> (SavedState, Option<String>) {
    match std::fs::read(state_path(workspace)) {
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
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("state path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub fn remember(workspace: &Path, model: &str) -> std::io::Result<()> {
    update(workspace, |state| state.model = Some(model.into()))
}
fn update(workspace: &Path, edit: impl FnOnce(&mut SavedState)) -> std::io::Result<()> {
    static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WRITER.lock().unwrap_or_else(|e| e.into_inner());
    let (mut state, _) = load(workspace);
    edit(&mut state);
    atomic_write(&state_path(workspace), &serde_json::to_vec(&state)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_failure_preserves_existing_path() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("not-a-directory");
        std::fs::write(&blocker, b"preserved").unwrap();
        assert!(atomic_write(&blocker.join("state.json"), b"replacement").is_err());
        assert_eq!(std::fs::read(blocker).unwrap(), b"preserved");
    }
}
