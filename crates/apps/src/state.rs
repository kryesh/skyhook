//! A product's saved UI state for a workspace: the remembered model and mode, and the
//! terminal's preferences.
use serde::{Deserialize, Deserializer, Serialize};
use skyhook::{config::mode::ModeName, provider::model::ModelRef};
use skyhook_worker::fs::{CommitMode, PermissionPolicy, StagedFile};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

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

/// The process's one copy of the saved UI state and the file that keeps it, written by
/// one writer in the order its saves were made.
pub struct Prefs {
    pub path: PathBuf,
    pub saved: RefCell<SavedState>,
    writer: Arc<Mutex<Writer>>,
}

/// Told how the write that holds a save went.
type Saved = Box<dyn FnOnce(Result<(), &std::io::Error>) + Send>;

enum Writer {
    Idle,
    /// A write is under way; saves made since it began wait for the next, which holds
    /// the newest of them.
    Writing(Option<(SavedState, Vec<Saved>)>),
}

impl Prefs {
    /// Read `file` of the workspace's `.skyhook` directory, independent of the shared
    /// session directory; its defaults apply where it cannot be read.
    pub fn load(workspace: &Path, file: &str) -> (Self, Option<Unreadable>) {
        let path = skyhook::config::workspace_directory(workspace).join(file);
        let (saved, unreadable) = load(&path);
        let saved = RefCell::new(saved);
        let writer = Arc::new(Mutex::new(Writer::Idle));
        (
            Self {
                path,
                saved,
                writer,
            },
            unreadable,
        )
    }

    /// Write the settings as they are now in the background, after every earlier save.
    pub fn save(&self, saved: impl FnOnce(Result<(), &std::io::Error>) + Send + 'static) {
        let snapshot = self.saved.borrow().clone();
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *writer {
            Writer::Idle => {
                *writer = Writer::Writing(None);
                let (path, writer) = (self.path.clone(), self.writer.clone());
                tokio::task::spawn_blocking(move || {
                    write_from(&path, &writer, snapshot, vec![Box::new(saved)]);
                });
            }
            Writer::Writing(next) => {
                let mut waiting = next.take().map(|(_, waiting)| waiting).unwrap_or_default();
                waiting.push(Box::new(saved));
                *next = Some((snapshot, waiting));
            }
        }
    }
}

/// Write `snapshot`, then each newest snapshot saved meanwhile, until none waits.
fn write_from(
    path: &Path,
    writer: &Mutex<Writer>,
    mut snapshot: SavedState,
    mut waiting: Vec<Saved>,
) {
    loop {
        let written = write(path, &snapshot);
        // The writer moves on before the write's saves are told, so the last one told
        // finds it idle.
        let next = match &mut *writer.lock().unwrap_or_else(PoisonError::into_inner) {
            Writer::Writing(next @ Some(_)) => next.take(),
            state => {
                *state = Writer::Idle;
                None
            }
        };
        for saved in waiting {
            saved(written.as_ref().copied());
        }
        let Some(next) = next else { return };
        (snapshot, waiting) = next;
    }
}
/// Why the saved settings could not be read; their defaults apply instead.
#[derive(Debug, thiserror::Error)]
pub enum Unreadable {
    #[error("Could not read UI state: {0}")]
    Read(#[from] std::io::Error),
    #[error("Could not read UI state: {0}")]
    Parse(#[from] serde_json::Error),
}

pub fn load(path: &Path) -> (SavedState, Option<Unreadable>) {
    let read = std::fs::read(path).map_err(Unreadable::from);
    match read.and_then(|bytes| Ok(serde_json::from_slice(&bytes)?)) {
        Ok(state) => (state, None),
        Err(Unreadable::Read(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            (SavedState::default(), None)
        }
        Err(error) => (SavedState::default(), Some(error)),
    }
}
fn write(path: &Path, state: &SavedState) -> std::io::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let mut staged = StagedFile::create(path, PermissionPolicy::Private)?;
    staged.write(&serde_json::to_vec(state)?)?;
    Ok(staged.commit(CommitMode::Replace)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Prefs {
        /// Whether every save made so far is written.
        pub(crate) fn written(&self) -> bool {
            matches!(*self.writer.lock().unwrap(), Writer::Idle)
        }
    }

    #[test]
    fn invalid_saved_names_drop_alone_and_the_sidebar_defaults_on() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        assert!(load(&path).0.sidebar);
        let saved = br#"{"model":"m","mode":" ","sidebar":false}"#;
        std::fs::write(&path, saved).unwrap();
        let (state, warning) = load(&path);
        assert!(state.model.is_none() && !state.sidebar && state.mode.is_none());
        assert!(warning.is_none());
    }
}
