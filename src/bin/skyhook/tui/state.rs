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
pub fn state_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("skyhook/ui.json")
}
pub fn load() -> (SavedState, Option<String>) {
    match std::fs::read(state_path()) {
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
pub fn remember(model: &str) -> std::io::Result<()> {
    update(|state| state.model = Some(model.into()))
}
fn update(edit: impl FnOnce(&mut SavedState)) -> std::io::Result<()> {
    static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WRITER.lock().unwrap_or_else(|e| e.into_inner());
    let (mut state, _) = load();
    edit(&mut state);
    atomic_write(&state_path(), &serde_json::to_vec(&state)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_failure_preserves_existing_path() {
        let root = tempfile::tempdir().unwrap();
        let blocker = root.path().join("not-a-directory");
        std::fs::write(&blocker, b"preserved").unwrap();
        assert!(atomic_write(&blocker.join("ui.json"), b"replacement").is_err());
        assert_eq!(std::fs::read(blocker).unwrap(), b"preserved");
    }
}
