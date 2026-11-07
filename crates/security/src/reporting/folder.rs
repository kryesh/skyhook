//! A report's folder on disk: checked against its layout, and rebuilt beside it and swapped in
//! when it differs.

use super::*;

/// `path` below `root`: `f` called with the directory holding its last component and that
/// component, each directory on the way opened without following a link.
pub(super) fn within<T>(
    root: &Directory,
    path: &str,
    f: impl FnOnce(&Directory, &str) -> Result<T>,
) -> Result<T> {
    match path.split_once('/') {
        None => f(root, path),
        Some((first, rest)) => within(&root.open_dir(first.as_ref())?, rest, f),
    }
}

/// Bring the root's folder in line with what renders: kept when it holds exactly that,
/// otherwise built afresh beside it from the store and the run's artifacts, each artifact
/// refused if it no longer matches what the finding recorded, then swapped in for it in one
/// step. A folder replaced or left half-built goes.
pub(super) fn project(
    reports: &Directory,
    artifacts: &Path,
    document: &Document,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let name = document.root.to_string();
    let layout = layout(document)?;
    if let Ok(folder) = reports.open_dir(name.as_ref())
        && holds(&folder, &layout, cancelled)?
    {
        return Ok(());
    }
    let staging = skyhook::fs::staging_name(name.as_ref())?;
    wanted(cancelled)?;
    let built = (|| -> Result<()> {
        let folder = reports.create_dir(staging.as_ref())?;
        for path in &layout.folders {
            within(&folder, path, |parent, name| {
                Ok(parent.create_dir(name.as_ref()).map(drop)?)
            })?;
        }
        for (path, content) in &layout.files {
            wanted(cancelled)?;
            within(&folder, path, |parent, name| {
                match content {
                    Content::Text(bytes) => parent.write_new(name.as_ref(), bytes.as_slice())?,
                    Content::Artifact(artifact) => {
                        copy_artifact(parent, name, artifacts, artifact, cancelled)?
                    }
                }
                Ok(parent.sync()?)
            })?;
        }
        folder.sync()?;
        wanted(cancelled)?;
        match reports.exchange(staging.as_ref(), name.as_ref()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                reports.rename(&staging, name.as_ref())?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(reports.sync()?)
    })();
    // After a swap the staging name holds the folder it replaced.
    match reports.remove_tree(staging.as_ref()) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => built.and(Err(error.into())),
        _ => built,
    }
}

/// Copy one recorded artifact into `directory` as `name`, refusing it once its contents no
/// longer match the digest its test recorded.
pub(super) fn copy_artifact(
    directory: &Directory,
    name: &str,
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
        directory.write_new(name.as_ref(), &mut reader)?;
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
pub(super) fn holds(
    folder: &Directory,
    layout: &Layout,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool> {
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
    fn below(
        &self,
        folder: &Directory,
        prefix: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let Ok(entries) = folder.entries() else {
            return Ok(false);
        };
        if entries.len() != self.children.get(prefix).copied().unwrap_or_default() {
            return Ok(false);
        }
        for entry in entries {
            wanted(cancelled)?;
            let Some(name) = entry.to_str() else {
                return Ok(false);
            };
            let path = format!("{prefix}{name}");
            let same = if self.layout.folders.contains(&path) {
                match folder.open_dir(name.as_ref()) {
                    Ok(child) => self.below(&child, &format!("{path}/"), cancelled)?,
                    Err(_) => false,
                }
            } else if let Some(content) = self.files.get(path.as_str()) {
                same(folder, name, content, cancelled)?
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

/// Whether file `name` below `directory` holds `content`: the rendered text, read until it
/// first differs, or the artifact as its test recorded it.
pub(super) fn same(
    directory: &Directory,
    name: &str,
    content: &Content,
    cancelled: &dyn Fn() -> bool,
) -> Result<bool> {
    let Ok(file) = directory.open_file(name.as_ref()) else {
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

/// Replace `name` below `directory` with `bytes` unless it already holds them.
pub(super) fn replace_changed(
    directory: &Directory,
    name: &str,
    bytes: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    if let Ok(file) = directory.open_file(name.as_ref())
        && equal(file, bytes, cancelled)?
    {
        return Ok(());
    }
    wanted(cancelled)?;
    Ok(directory.replace(name.as_ref(), bytes)?)
}
