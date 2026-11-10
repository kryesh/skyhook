//! Reading, writing and removing this machine's files.
use std::{io::Read as _, path::Path};

use tokio::fs;

use crate::diagnostic::{Effects, Failure, Fault, Operation, Subject};
use crate::fs::{Contents, FileKind};
use crate::op::{
    Content, CreateDirs, Directories, Entry, EntryKind, Events, Extent, FsError, Input, Never,
    OsError, Read, ReadEnd, ReadEvent, ReadFrom, ReadMode, Remove, RemoveKind, TargetPath, Write,
    Written,
};

use crate::op::CHUNK_BYTES;

impl crate::op::Operation for Read {
    type Event = ReadEvent;
    type End = ReadEnd;
    type Error = Failure;

    fn paths(&mut self) -> Vec<&mut TargetPath> {
        vec![&mut self.path]
    }

    async fn perform(
        self,
        _: &super::Local,
        events: Events<ReadEvent>,
    ) -> Result<ReadEnd, Failure> {
        let read = self;
        let path = read.path.native();
        let failed = |operation| {
            let subject = Subject::path(&read.path);
            move |error: FsError| Failure::at(operation, subject, error)
        };
        let counts = match read.mode {
            ReadMode::Entry { counts } => Some(counts),
            ReadMode::File => None,
        };
        if counts.is_some()
            && (fs::metadata(path).await)
                .map_err(|error| failed(Operation::Inspect)(error.into()))?
                .is_dir()
        {
            list(&read.path, &events).await?;
            return Ok(ReadEnd::Directory);
        }
        let file =
            (crate::fs::open_regular(path, u64::MAX).await).map_err(failed(Operation::Read))?;
        let from = read.from;
        skyhook_base::blocking(move || stream(file, (from, counts), &events))
            .await
            .map_err(|error| failed(Operation::Read)(error.into()))?;
        let shown = super::search::relative_path(read.workspace.native(), path);
        Ok(ReadEnd::File { shown })
    }
}

/// Where `from` is in `file`, which is left positioned there; a broken pipe once
/// nobody waits for the read's `events` any more.
fn seek(
    file: &mut crate::fs::OpenFile,
    from: ReadFrom,
    events: &Events<ReadEvent>,
) -> std::io::Result<u64> {
    // A line starts after as many line ends as there are lines before it.
    let mut ahead = match from {
        ReadFrom::Start => 0,
        ReadFrom::Line(line) => line.saturating_sub(1),
    };
    let (mut offset, mut chunk) = (0, vec![0; 2 * CHUNK_BYTES]);
    while ahead > 0 {
        if events.is_closed() {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let mut passed = read;
        let ends = (chunk[..read].iter().enumerate()).filter(|(_, byte)| **byte == b'\n');
        for (end, _) in ends {
            ahead -= 1;
            if ahead == 0 {
                passed = end + 1;
                break;
            }
        }
        offset += passed as u64;
    }
    std::io::Seek::seek(file, std::io::SeekFrom::Start(offset))
}

/// All that `file` holds, which is left at its start: none unless all of it is text,
/// UTF-8 with no NUL byte. A broken pipe once nobody waits for the read's `events`.
fn count(
    file: &mut crate::fs::OpenFile,
    events: &Events<ReadEvent>,
) -> std::io::Result<Option<Extent>> {
    let (mut extent, mut chunk) = (Extent::default(), vec![0; 2 * CHUNK_BYTES]);
    // The bytes at the chunk's start that begin a character the last read split.
    let mut split = 0;
    let counted = loop {
        if events.is_closed() {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let read = file.read(&mut chunk[split..])?;
        let held = split + read;
        let whole = match std::str::from_utf8(&chunk[..held]) {
            Ok(_) => held,
            // A character the file's end splits is no text.
            Err(error) if error.error_len().is_none() && read > 0 => error.valid_up_to(),
            Err(_) => break None,
        };
        if chunk[split..held].contains(&0) {
            break None;
        }
        if read == 0 {
            break Some(extent);
        }
        extent = extent.after(&chunk[split..held]);
        chunk.copy_within(whole..held, 0);
        split = held - whole;
    };
    std::io::Seek::rewind(file)?;
    Ok(counted)
}

/// Send `file`'s bytes from `from` on as events, after where they start and, of a
/// file longer than `counts` bytes, all that it holds.
fn stream(
    mut file: crate::fs::OpenFile,
    (from, counts): (ReadFrom, Option<u64>),
    events: &Events<ReadEvent>,
) -> std::io::Result<()> {
    let length = file.metadata()?.len();
    let counted = match counts {
        Some(counts) if length > counts => count(&mut file, events)?,
        _ => None,
    };
    let offset = seek(&mut file, from, events)?;
    let opened = ReadEvent::Opened {
        length,
        offset,
        counted,
    };
    if events.send_blocking(opened).is_err() {
        return Ok(());
    }
    loop {
        let mut chunk = vec![0; CHUNK_BYTES];
        let read = file.read(&mut chunk)?;
        chunk.truncate(read);
        // Nobody waits for the rest once the read is being dropped.
        if read == 0 || events.send_blocking(ReadEvent::Bytes(chunk)).is_err() {
            return Ok(());
        }
    }
}

/// Send a directory's entries, each classified by its own metadata.
async fn list(path: &TargetPath, events: &Events<ReadEvent>) -> Result<(), Failure> {
    let listing = |error| Failure::at(Operation::ReadDirectory, Subject::path(path), error);
    let mut directory = fs::read_dir(path.native()).await.map_err(listing)?;
    let mut entries = super::Batch::default();
    while let Some(entry) = directory.next_entry().await.map_err(listing)? {
        let metadata = fs::symlink_metadata(entry.path()).await.map_err(|error| {
            let entry = TargetPath::new(entry.path().to_string_lossy());
            Failure::at(Operation::Inspect, Subject::DirectoryEntry(entry), error)
        })?;
        let kind = match FileKind::from(metadata.file_type()) {
            FileKind::File => EntryKind::File {
                bytes: metadata.len(),
            },
            FileKind::Directory => EntryKind::Directory,
            FileKind::Symlink => EntryKind::Symlink,
            FileKind::Other => EntryKind::Other,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let text = name.len();
        if let Some(full) = entries.push(Entry { name, kind }, text) {
            let _ = events.send(ReadEvent::Entries(full)).await;
        }
    }
    let rest = entries.take();
    if !rest.is_empty() {
        let _ = events.send(ReadEvent::Entries(rest)).await;
    }
    Ok(())
}

impl crate::op::Operation for Write {
    type Event = Never;
    type End = Written;
    type Error = Failure;

    fn input(&mut self) -> Option<&mut Input> {
        match &mut self.content {
            Content::Streamed(input) => Some(input),
            Content::Inline(_) | Content::Copy(_) => None,
        }
    }

    fn paths(&mut self) -> Vec<&mut TargetPath> {
        vec![&mut self.path]
    }

    async fn perform(self, _: &super::Local, _: Events<Never>) -> Result<Written, Failure> {
        let write = self;
        let path = write.path.native();
        let parents = match path.parent() {
            Some(parent) => create_dirs(parent).await.map_err(|error| {
                Failure::parents(TargetPath::new(parent.to_string_lossy()), error)
            })?,
            None => Directories::Existed,
        };
        let contents = match write.content {
            Content::Inline(bytes) => Contents::Bytes(bytes),
            Content::Streamed(input) => Contents::Reader(Box::new(
                tokio_util::io::SyncIoBridge::new(input.into_reader()),
            )),
            // Only the tool names its source's argument.
            Content::Copy(source) => Contents::File(
                (crate::fs::open_regular(source.native(), u64::MAX).await)
                    .map_err(|error| Failure::from(Fault::from(error)).left_by(parents))?,
            ),
        };
        let bytes = (crate::fs::atomic_write(path, contents).await)
            .map_err(|error| Failure::atomic(&write.path, error).left_by(parents))?;
        Ok(Written { bytes })
    }
}

impl crate::op::Operation for CreateDirs {
    type Event = Never;
    type End = Directories;
    type Error = OsError;

    async fn perform(self, _: &super::Local, _: Events<Never>) -> Result<Directories, OsError> {
        create_dirs(self.path.native()).await
    }
}

/// Create `directory` and the missing directories above it.
pub async fn create_dirs(directory: &Path) -> Result<Directories, OsError> {
    let directory = directory.to_owned();
    skyhook_base::blocking(move || {
        if std::fs::metadata(&directory).is_ok_and(|found| found.is_dir()) {
            return Ok(Directories::Existed);
        }
        std::fs::create_dir_all(&directory)?;
        Ok(Directories::Created)
    })
    .await
}

impl crate::op::Operation for Remove {
    type Event = Never;
    type End = RemoveKind;
    type Error = Failure;

    fn paths(&mut self) -> Vec<&mut TargetPath> {
        vec![&mut self.path]
    }

    async fn perform(self, _: &super::Local, _: Events<Never>) -> Result<RemoveKind, Failure> {
        let remove = self;
        let unchanged =
            |operation, fault: Fault| Failure::unchanged(operation, &remove.path, fault);
        let path = remove.path.native().to_owned();
        let metadata = (fs::symlink_metadata(&path).await)
            .map_err(|error| unchanged(Operation::Inspect, error.into()))?;
        let kind = match FileKind::from(metadata.file_type()) {
            FileKind::Symlink => RemoveKind::Symlink,
            FileKind::File => RemoveKind::File,
            FileKind::Directory => RemoveKind::Directory,
            FileKind::Other => return Err(unchanged(Operation::Remove, Fault::UnsupportedEntry)),
        };
        let removing =
            |fault: Fault| Failure::at(Operation::Remove, Subject::path(&remove.path), fault);
        // A live database refuses the removal before anything is deleted; only an
        // I/O failure partway through a tree leaves part of it removed.
        let guarded = async |remove: fn(&Path) -> Result<(), FsError>, partial: bool| {
            let target = path.clone();
            match skyhook_base::blocking(move || remove(&target)).await {
                Ok(()) => Ok(()),
                Err(FsError::Os(error)) if partial => {
                    Err(removing(Fault::Os(error)).effects(Effects::PartialChange))
                }
                Err(FsError::Os(error)) => Err(removing(Fault::Os(error))),
                Err(_) => Err(unchanged(Operation::Remove, Fault::Live)),
            }
        };
        match kind {
            RemoveKind::Directory if remove.recursive => {
                guarded(crate::fs::remove_tree_unless_live, true).await?
            }
            RemoveKind::Directory => {
                let target = path.clone();
                let removed = skyhook_base::blocking(move || std::fs::remove_dir(target)).await;
                removed.map_err(|error| {
                    removing(match error.kind() {
                        std::io::ErrorKind::DirectoryNotEmpty => Fault::NotEmpty(error.into()),
                        _ => error.into(),
                    })
                })?
            }
            RemoveKind::Symlink | RemoveKind::File => {
                guarded(crate::fs::remove_file_unless_live, false).await?
            }
        }
        Ok(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read nobody waits for any more stops looking for its line.
    #[test]
    fn a_seek_stops_once_its_read_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lines");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let open = || crate::fs::open_regular_blocking(&path, u64::MAX).unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let events = Events::new(sender);
        assert_eq!(seek(&mut open(), ReadFrom::Line(3), &events).unwrap(), 8);
        drop(receiver);
        let dropped = seek(&mut open(), ReadFrom::Line(3), &events).unwrap_err();
        assert_eq!(dropped.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// A file is counted as text only when all of it is UTF-8, wherever a read
    /// splits a character.
    #[test]
    fn a_file_is_counted_only_when_all_of_it_is_text() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("text");
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let events = Events::new(sender);
        let counted = |bytes: &[u8]| {
            std::fs::write(&path, bytes).unwrap();
            let mut file = crate::fs::open_regular_blocking(&path, u64::MAX).unwrap();
            count(&mut file, &events).unwrap()
        };
        // Its reads end inside two-byte characters.
        let text = format!("ab\n{}", "é".repeat(3 * CHUNK_BYTES)).into_bytes();
        let extent = counted(&text).unwrap();
        assert_eq!((extent.bytes, extent.newlines), (text.len() as u64, 1));
        for suffix in [&b"\xff"[..], &"é".as_bytes()[..1]] {
            assert_eq!(counted(&[&text[..], suffix].concat()), None);
        }
    }
}
