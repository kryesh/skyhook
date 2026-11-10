//! This machine as a target: every operation run here, on its files, processes
//! and network, under one authorization root.

pub(crate) mod exec;
pub(crate) mod fetch;
pub(crate) mod files;
pub(crate) mod relay;
pub(crate) mod replace;
mod resolve;
pub(crate) mod search;

use std::{path::PathBuf, sync::Arc, time::Duration};

use futures_util::future::BoxFuture;

use crate::{
    op::{Events, Running},
    target::Target,
};

/// How a deadline elapses: `tokio::time::sleep`, or a test's own clock.
pub type Sleep = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// The events an operation may have in flight before its consumer takes them.
pub(crate) const EVENTS: usize = 16;

/// Matched lines, directory entries or paths gathered into events of about a chunk's
/// bytes each, so a window of frames is a bound on bytes.
struct Batch<T> {
    items: Vec<T>,
    bytes: usize,
}

impl<T> Default for Batch<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            bytes: 0,
        }
    }
}

impl<T> Batch<T> {
    /// What each item takes in an event beyond the text it names.
    const ITEM_BYTES: usize = 16;

    /// Add `item`, naming `text` bytes of text; the batch before it when it would
    /// pass a chunk.
    fn push(&mut self, item: T, text: usize) -> Option<Vec<T>> {
        let bytes = text + Self::ITEM_BYTES;
        let full = (self.bytes + bytes > crate::op::CHUNK_BYTES && !self.items.is_empty())
            .then(|| self.take());
        self.bytes += bytes;
        self.items.push(item);
        full
    }

    fn take(&mut self) -> Vec<T> {
        self.bytes = 0;
        std::mem::take(&mut self.items)
    }
}

/// This machine, resolving paths against its authorization root.
#[derive(Clone)]
pub struct Local(Arc<Machine>);

struct Machine {
    authorization_root: PathBuf,
    sleep: Sleep,
    /// Where its spills are kept, when its user has a cache directory.
    spills: Option<PathBuf>,
    /// The SSH origin this machine is for its connection.
    origin: crate::origin::ssh::Origin,
    /// The blocking work its calls started, which may outlive their tasks, and the
    /// SSH processes and WinRM shells its relays started, each until it is killed or
    /// deleted: its connection waits for them before it ends.
    work: skyhook_base::Work,
}

impl Local {
    /// This machine with `root`, which must already be canonical, as its authorization
    /// root.
    pub fn new(root: PathBuf) -> Self {
        Self::with_clock(
            root,
            Arc::new(|duration| Box::pin(tokio::time::sleep(duration))),
        )
    }

    /// [`new`](Self::new), with deadlines that elapse when `sleep` does.
    pub fn with_clock(authorization_root: PathBuf, sleep: Sleep) -> Self {
        Self(Arc::new(Machine {
            authorization_root,
            sleep,
            spills: crate::fs::spill::root(),
            origin: Default::default(),
            work: Default::default(),
        }))
    }

    /// This machine keeping its spills under `root`, where the session it serves keeps
    /// its own, instead of under its user's cache directory.
    pub fn spilling_in(mut self, root: Option<PathBuf>) -> Self {
        let machine = Arc::get_mut(&mut self.0).expect("a machine is placed before it is shared");
        machine.spills = root;
        self
    }

    /// This machine under one connection's `settings`, its root made canonical.
    pub fn open(settings: &crate::target::Settings) -> Result<Self, crate::target::SettingsError> {
        let root = &settings.authorization_root;
        let authorization_root = canonical(root, |error| {
            let path = root.clone();
            crate::target::SettingsError::AuthorizationRoot { path, error }
        })?;
        Ok(Self::new(authorization_root))
    }

    /// What this machine reports about itself, read now.
    pub fn facts(&self) -> crate::target::Facts {
        let text = |path: &std::path::Path| crate::op::TargetPath::new(path.to_string_lossy());
        crate::target::Facts {
            platform: crate::target::Platform::current(),
            zone: crate::target::TimeZone::local(),
            authorization_root: text(&self.0.authorization_root),
        }
    }

    pub(crate) fn authorization_root(&self) -> &std::path::Path {
        &self.0.authorization_root
    }

    /// Where spill `name` is kept here, its directory created.
    pub(crate) fn spill(
        &self,
        name: &crate::fs::spill::SpillName,
    ) -> Result<crate::fs::spill::SpillFile, crate::fs::spill::SpillError> {
        crate::fs::spill::place(self.0.spills.as_deref(), name)
    }

    pub(crate) fn origin(&self) -> &crate::origin::ssh::Origin {
        &self.0.origin
    }

    pub(crate) fn work(&self) -> &skyhook_base::Work {
        &self.0.work
    }

    pub(crate) fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        (self.0.sleep)(duration)
    }
}

impl Target for Local {
    type Unreachable = std::convert::Infallible;

    async fn run<O: crate::protocol::Wire>(
        &self,
        operation: O,
    ) -> Result<Running<O, std::convert::Infallible>, std::convert::Infallible> {
        let (sender, receiver) = tokio::sync::mpsc::channel(EVENTS);
        let local = self.clone();
        let task =
            tokio::spawn(async move { operation.perform(&local, Events::new(sender)).await });
        // Dropping the running operation drops its end, and with it the task.
        let task = tokio_util::task::AbortOnDropHandle::new(task);
        let end = async move { Ok(skyhook_base::joined(task.await).await) };
        Ok(Running::new(receiver, end, ()))
    }
}

/// `path`, as a setting spells it, made canonical, or what `failed` makes of why it
/// cannot be. A relative path is in the home directory, where a login starts.
fn canonical(
    path: &str,
    failed: impl FnOnce(crate::op::OsError) -> crate::target::SettingsError,
) -> Result<PathBuf, crate::target::SettingsError> {
    use crate::target::SettingsError;
    let path = std::path::Path::new(path);
    let path = match path.is_relative() {
        true => std::env::home_dir()
            .ok_or(SettingsError::NoHome)?
            .join(path),
        false => path.to_owned(),
    };
    let canonical = crate::fs::canonical(&path).map_err(|error| failed(error.into()))?;
    match canonical.to_str() {
        Some(_) => Ok(canonical),
        None => Err(SettingsError::Unspelled {
            path: canonical.to_string_lossy().into_owned(),
        }),
    }
}

impl crate::op::Operation for crate::op::Workspace {
    type Event = crate::op::Never;
    type End = crate::op::TargetPath;
    type Error = crate::target::SettingsError;

    async fn perform(
        self,
        _: &Local,
        _: Events<crate::op::Never>,
    ) -> Result<crate::op::TargetPath, crate::target::SettingsError> {
        skyhook_base::blocking(move || {
            let joined: PathBuf = self
                .segments
                .iter()
                .map(crate::op::TargetPath::as_str)
                .collect();
            let path = joined.to_string_lossy().into_owned();
            let workspace = canonical(&path, |error| {
                let path = path.clone();
                crate::target::SettingsError::Workspace { path, error }
            })?;
            Ok(crate::op::TargetPath::new(workspace.to_string_lossy()))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::target::{Settings, SettingsError, Target as _};

    /// A relative setting is in the home directory, where a login starts, and a
    /// workspace's segments join in order, an absolute one replacing those before; a
    /// workspace that is not there is refused, naming it as joined.
    #[tokio::test]
    async fn settings_resolve_from_the_home_directory_and_join_in_order() {
        let home = crate::fs::canonical(&std::env::home_dir().unwrap()).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let elsewhere = crate::fs::canonical(elsewhere.path()).unwrap();
        let text = |path: &std::path::Path| path.to_str().unwrap().to_owned();
        let settings = Settings {
            authorization_root: ".".into(),
        };
        let local = super::Local::open(&settings).unwrap();
        assert_eq!(local.facts().authorization_root.as_str(), text(&home));
        let workspace = |segments: Vec<String>| crate::op::Workspace {
            segments: segments.into_iter().map(Into::into).collect(),
        };
        let Ok(joined) = local
            .call(workspace(vec![".".into(), text(&elsewhere)]))
            .await;
        assert_eq!(joined.unwrap().as_str(), text(&elsewhere));
        let missing = text(&elsewhere.join("missing"));
        let Ok(refused) = local.call(workspace(vec![missing.clone()])).await;
        assert!(matches!(refused, Err(SettingsError::Workspace { path, .. }) if path == missing));
    }

    /// A batch fills to about a chunk's bytes however small its items, and an item
    /// that alone passes the bound still starts a batch of its own.
    #[test]
    fn batches_are_bounded_by_bytes() {
        let mut batch = super::Batch::default();
        let mut full = Vec::new();
        let large = crate::op::CHUNK_BYTES;
        for text in std::iter::repeat_n(0, 5000).chain([large, 1]) {
            full.extend(batch.push(text, text).map(|items| items.len()));
        }
        let per_chunk = crate::op::CHUNK_BYTES / super::Batch::<usize>::ITEM_BYTES;
        assert_eq!(full, [per_chunk, per_chunk, 5000 - 2 * per_chunk, 1]);
        assert_eq!(batch.take(), [1]);
    }
}
