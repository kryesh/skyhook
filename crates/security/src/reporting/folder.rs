//! A report's folder on disk: checked against its layout, and rebuilt beside it and moved in
//! when it differs.

use std::fs;

use skyhook_worker::fs::{
    CommitMode, PermissionPolicy, StagedFile, open_regular_blocking, sync_directory,
};

use super::*;

/// Create `reports` when missing, its entry durable in its parent, refusing one that is a
/// symbolic link.
pub(super) fn open_root(reports: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    match fs::DirBuilder::new().mode(0o700).create(reports) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error.into()),
        _ => {}
    }
    if fs::symlink_metadata(reports)?.is_symlink() {
        return Err(Error::LinkedRoot);
    }
    let parent = (reports.parent()).filter(|parent| !parent.as_os_str().is_empty());
    Ok(sync_directory(parent.unwrap_or(Path::new(".")))?)
}

/// Remove `path` with everything below it; a link is removed, never followed.
pub(super) fn remove_entry(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// An empty private folder below `reports` under a hidden name of `name`'s, removed with
/// everything in it when dropped, which also clears it after a step that failed.
fn beside(reports: &Path, name: &str) -> io::Result<tempfile::TempDir> {
    (tempfile::Builder::new())
        .prefix(&format!(".{name}."))
        .suffix(".tmp")
        .tempdir_in(reports)
}

/// Write a new private file at `path` from `contents`, never replacing an existing entry.
fn write_new(path: &Path, mut contents: impl io::Read) -> Result<()> {
    let staged = StagedFile::create(path, PermissionPolicy::Private)?;
    io::copy(&mut contents, &mut staged.as_file())?;
    Ok(staged.commit(CommitMode::NoClobber)?)
}

/// Bring the root's folder in line with what renders: kept when it holds exactly that,
/// otherwise built afresh beside it from the store and the run's artifacts, each artifact
/// refused if it no longer matches what the finding recorded, then moved in: the old folder
/// is moved aside first and back again if the new one cannot follow. A folder replaced or
/// left half-built goes.
pub(super) fn project(
    reports: &Path,
    artifacts: &Path,
    document: &Document,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let name = document.root.to_string();
    let target = reports.join(&name);
    let layout = layout(document)?;
    let current = fs::symlink_metadata(&target).ok();
    if current.as_ref().is_some_and(fs::Metadata::is_dir) && holds(&target, &layout, cancelled)? {
        return Ok(());
    }
    wanted(cancelled)?;
    let staging = beside(reports, &name)?;
    let folder = staging.path();
    for path in &layout.folders {
        fs::create_dir(folder.join(path))?;
    }
    for (path, content) in &layout.files {
        wanted(cancelled)?;
        let path = folder.join(path);
        match content {
            Content::Text(bytes) => write_new(&path, bytes.as_slice())?,
            Content::Artifact(artifact) => copy_artifact(&path, artifacts, artifact, cancelled)?,
        }
    }
    sync_directory(folder)?;
    wanted(cancelled)?;
    let aside = match current {
        Some(current) if current.is_dir() => {
            let aside = beside(reports, &name)?;
            fs::rename(&target, aside.path())?;
            Some(aside)
        }
        Some(_) => {
            fs::remove_file(&target)?;
            None
        }
        None => None,
    };
    if let Err(error) = fs::rename(folder, &target) {
        if let Some(aside) = aside {
            fs::rename(aside.keep(), &target)?;
        }
        return Err(error.into());
    }
    sync_directory(reports)?;
    Ok(aside.map_or(Ok(()), tempfile::TempDir::close)?)
}

/// Copy one recorded artifact to `path`, refusing it once its contents no longer match the
/// digest its test recorded.
pub(super) fn copy_artifact(
    path: &Path,
    artifacts: &Path,
    artifact: &RecordedArtifact,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let recorded = TestArtifact {
        reference: artifact.reference.clone(),
        role: artifact.role,
    };
    for mut open in store::admit(artifacts, &[recorded])? {
        let mut reader = Digesting::new(open.contents()?, cancelled);
        write_new(path, &mut reader)?;
        if reader.finish() != artifact.digest {
            return Err(store::Error::Inadmissible(store::Inadmissible::Fault {
                reference: artifact.reference.clone(),
                fault: ArtifactFault::Changed,
            })
            .into());
        }
    }
    Ok(())
}

/// Whether `folder` holds exactly what `layout` names, each file as it renders or as the
/// finding recorded it. Entries are checked against the layout as they are listed, and the
/// first that differs ends the check.
pub(super) fn holds(folder: &Path, layout: &Layout, cancelled: &dyn Fn() -> bool) -> Result<bool> {
    let files: BTreeMap<&str, &Content> = (layout.files.iter())
        .map(|(path, content)| (path.as_str(), content))
        .collect();
    let mut children = BTreeMap::<&str, usize>::new();
    for path in (layout.folders.iter().map(String::as_str)).chain(files.keys().copied()) {
        let parent = path.rfind('/').map_or("", |slash| &path[..=slash]);
        *children.entry(parent).or_default() += 1;
    }
    let expected = Expected {
        layout,
        files,
        children,
    };
    expected.below(folder, "", cancelled)
}

/// What a report's folder should hold: its folders, its files, and how many entries each
/// folder holds directly, by the path prefix of its entries.
pub(super) struct Expected<'a> {
    pub(super) layout: &'a Layout<'a>,
    pub(super) files: BTreeMap<&'a str, &'a Content<'a>>,
    pub(super) children: BTreeMap<&'a str, usize>,
}

impl Expected<'_> {
    /// Whether the directory `folder` holds what the layout names below `prefix`; an entry's
    /// own kind decides, so a link is never followed.
    fn below(&self, folder: &Path, prefix: &str, cancelled: &dyn Fn() -> bool) -> Result<bool> {
        let Ok(entries) = fs::read_dir(folder).and_then(Iterator::collect::<io::Result<Vec<_>>>)
        else {
            return Ok(false);
        };
        if entries.len() != self.children.get(prefix).copied().unwrap_or_default() {
            return Ok(false);
        }
        for entry in entries {
            wanted(cancelled)?;
            let (Ok(name), Ok(kind)) = (entry.file_name().into_string(), entry.file_type()) else {
                return Ok(false);
            };
            let path = format!("{prefix}{name}");
            let same = if self.layout.folders.contains(&path) {
                kind.is_dir() && self.below(&entry.path(), &format!("{path}/"), cancelled)?
            } else if let Some(content) = self.files.get(path.as_str()) {
                kind.is_file() && same(&entry.path(), content, cancelled)?
            } else {
                false
            };
            if !same {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Whether the regular file at `path` holds `content`: the rendered text, read until it
/// first differs, or the artifact as its test recorded it.
pub(super) fn same(path: &Path, content: &Content, cancelled: &dyn Fn() -> bool) -> Result<bool> {
    let Ok(file) = open_regular_blocking(path, u64::MAX) else {
        return Ok(false);
    };
    match content {
        Content::Text(bytes) => equal(file, bytes, cancelled),
        Content::Artifact(artifact) => {
            let mut digest = Digesting::new(file, cancelled);
            match io::copy(&mut digest, &mut io::sink()).map_err(|error| stopped(error.into())) {
                Ok(_) => Ok(digest.finish() == artifact.digest),
                Err(Error::Cancelled) => Err(Error::Cancelled),
                Err(_) => Ok(false),
            }
        }
    }
}

/// Whether `file` holds exactly `expected`, compared chunk by chunk.
pub(super) fn equal(
    mut file: impl io::Read,
    expected: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<bool> {
    let mut chunk = [0; 8192];
    let mut at = 0;
    loop {
        wanted(cancelled)?;
        let read = match file.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(false),
        };
        if read == 0 {
            return Ok(at == expected.len());
        }
        if expected.get(at..at + read) != Some(&chunk[..read]) {
            return Ok(false);
        }
        at += read;
    }
}

/// Replace the file at `path` with `bytes` unless it already holds them.
pub(super) fn replace_changed(
    path: &Path,
    bytes: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    if let Ok(file) = open_regular_blocking(path, u64::MAX)
        && equal(file, bytes, cancelled)?
    {
        return Ok(());
    }
    wanted(cancelled)?;
    let mut staged = StagedFile::create(path, PermissionPolicy::Private)?;
    staged.write(bytes)?;
    Ok(staged.commit(CommitMode::Replace)?)
}
