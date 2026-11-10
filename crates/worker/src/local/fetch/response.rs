//! Bounded response streaming and atomic downloads.
use futures_util::StreamExt;
use reqwest::Method;

use super::{os, request::transport_error, step};
use crate::diagnostic::{Failure, Fault, Operation, Subject};
use crate::fs::spill::{SpillFile, SpillName};
use crate::fs::{CommitMode, PermissionPolicy, StagedWriter};
use crate::op::{
    CHUNK_BYTES, Directories, DownloadStep, Events, FetchEvent, FetchOutput, FetchStep, OsError,
    TargetPath,
};

async fn download_step(events: &Events<FetchEvent>, download: DownloadStep, path: &TargetPath) {
    let path = path.clone();
    step(
        events,
        FetchStep::Download {
            step: download,
            path,
        },
    )
    .await;
}

/// A destination exists only as a regular file it may replace.
pub(super) async fn check_destination(
    destination: &TargetPath,
    overwrite: bool,
    events: &Events<FetchEvent>,
) -> Result<(), Fault> {
    download_step(events, DownloadStep::Inspect, destination).await;
    let path = destination.native().to_owned();
    match skyhook_base::blocking(move || std::fs::metadata(path)).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(os(error)),
        Ok(_) if !overwrite => Err(Fault::SaveExists),
        Ok(found) if !found.is_file() => Err(Fault::SaveNotFile),
        Ok(_) => Ok(()),
    }
}

/// Owns the entire uncommitted download. Dropping it removes the staging file.
struct PendingDownload {
    staged: StagedWriter,
    place: Place,
    commit: CommitMode,
}

/// Whose place a download's file is in.
enum Place {
    /// Where its caller named: its steps and failures tell where they are.
    Named {
        destination: TargetPath,
        staging: TargetPath,
    },
    /// This machine's spill directory, which neither a step nor a failure tells of:
    /// only the fetch's end names the file.
    Spill(SpillFile),
}

/// A spill's file failed with `error`: all its job is told of it.
fn unkept(error: impl Into<OsError>) -> Failure {
    let error = OsError {
        detail: None,
        ..error.into()
    };
    Failure::at(Operation::Save, Subject::None, Fault::Os(error))
}

impl PendingDownload {
    /// A download to `destination`, whose missing parents it creates.
    async fn save(
        destination: TargetPath,
        overwrite: bool,
        events: &Events<FetchEvent>,
    ) -> Result<Self, Failure> {
        if let Some(parent) = destination.native().parent() {
            let created = crate::local::files::create_dirs(parent).await;
            let created = created.map_err(|error| {
                Failure::parents(TargetPath::new(parent.to_string_lossy()), error)
            })?;
            if created == Directories::Created {
                let _ = events.send(FetchEvent::CreatedParents).await;
            }
        }
        let commit = match overwrite {
            true => CommitMode::Replace,
            false => CommitMode::NoClobber,
        };
        download_step(events, DownloadStep::Stage, &destination).await;
        let staged = StagedWriter::create(destination.native(), PermissionPolicy::Inherit).await;
        let staged = staged.map_err(Fault::Os)?;
        let staging = TargetPath::new(staged.path().to_string_lossy());
        Ok(Self {
            staged,
            place: Place::Named {
                destination,
                staging,
            },
            commit,
        })
    }

    /// A body kept as `name` in this machine's spill directory, for its user alone.
    async fn spill(local: &crate::local::Local, name: SpillName) -> Result<Self, Failure> {
        let local = local.clone();
        let placed = skyhook_base::blocking(move || local.spill(&name)).await;
        let unplaced = |failed: crate::fs::spill::SpillError| {
            Failure::at(Operation::CreateDirectories, Subject::None, failed.fault())
        };
        let file = placed.map_err(unplaced)?;
        let staged = StagedWriter::create(file.path.native(), PermissionPolicy::Private).await;
        Ok(Self {
            staged: staged.map_err(unkept)?,
            place: Place::Spill(file),
            commit: CommitMode::NoClobber,
        })
    }

    /// Tell that the download reached `step`, when its caller named where it goes.
    async fn step(&self, events: &Events<FetchEvent>, step: DownloadStep) {
        if let Place::Named {
            destination,
            staging,
        } = &self.place
        {
            let path = match step {
                DownloadStep::Receive => destination,
                _ => staging,
            };
            download_step(events, step, path).await;
        }
    }

    /// How writing the download's file fails with `error`.
    fn failed(&self, error: std::io::Error) -> Failure {
        match self.place {
            Place::Named { .. } => os(error).into(),
            Place::Spill(_) => unkept(error),
        }
    }

    async fn write(&mut self, chunk: &[u8], events: &Events<FetchEvent>) -> Result<(), Failure> {
        self.step(events, DownloadStep::Write).await;
        let written = self.staged.write(chunk).await;
        written.map_err(|error| self.failed(error))
    }

    /// Sync before publishing; the spill file a returned body was kept in. Once the
    /// commit starts it runs to the end, so cancellation cannot undo a rename.
    async fn finish(mut self, events: &Events<FetchEvent>) -> Result<Option<SpillFile>, Failure> {
        self.step(events, DownloadStep::Sync).await;
        let synced = self.staged.sync().await;
        synced.map_err(|error| self.failed(error))?;
        let Self {
            staged,
            place,
            commit,
        } = self;
        if let Place::Named { destination, .. } = &place {
            step(events, FetchStep::Commit(destination.clone())).await;
        }
        match (place, staged.commit(commit).await) {
            (Place::Named { .. }, Ok(())) => Ok(None),
            (Place::Named { destination, .. }, Err(error)) => {
                Err(Failure::atomic(&destination, error))
            }
            (Place::Spill(file), Ok(())) => Ok(Some(file)),
            (Place::Spill(_), Err(error)) => Err(unkept(error.error)),
        }
    }
}

/// Where a body's bytes go as they arrive.
enum Body {
    /// Held to send back, while they are within `inline`; past it they are kept
    /// in `spill`.
    Held {
        bytes: Vec<u8>,
        inline: u64,
        spill: SpillName,
    },
    File(PendingDownload),
}

/// Stream the body to its output, failing past `max_bytes` decoded bytes of it when
/// that is given: the bytes it had, and the file a returned body was kept in when
/// it was longer than may be sent back.
pub(super) async fn read_body(
    response: reqwest::Response,
    output: FetchOutput,
    (max_bytes, method): (Option<u64>, &Method),
    local: &crate::local::Local,
    events: &Events<FetchEvent>,
) -> Result<(u64, Option<SpillFile>), Failure> {
    step(events, FetchStep::Receive).await;
    let exceeds = |bytes: u64| max_bytes.is_some_and(|limit| bytes > limit);
    if *method != Method::HEAD && response.content_length().is_some_and(exceeds) {
        return Err(Fault::ResponseTooLarge.into());
    }
    let mut body = match output {
        FetchOutput::Return { inline, spill } => Body::Held {
            bytes: Vec::new(),
            inline,
            spill,
        },
        FetchOutput::Save { path, overwrite } => {
            Body::File(PendingDownload::save(path, overwrite, events).await?)
        }
    };
    let mut stream = response.bytes_stream();
    let mut received = 0;
    loop {
        if let Body::File(download) = &body {
            download.step(events, DownloadStep::Receive).await;
        }
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk.map_err(transport_error)?;
        received += chunk.len() as u64;
        if exceeds(received) {
            return Err(Fault::ResponseTooLarge.into());
        }
        if let Body::Held {
            bytes,
            inline,
            spill,
        } = &mut body
        {
            if received <= *inline {
                bytes.extend_from_slice(&chunk);
                continue;
            }
            let mut download = PendingDownload::spill(local, spill.clone()).await?;
            download.write(bytes, events).await?;
            body = Body::File(download);
        }
        if let Body::File(download) = &mut body {
            download.write(&chunk, events).await?;
        }
    }
    match body {
        Body::Held { bytes, .. } => {
            for chunk in bytes.chunks(CHUNK_BYTES) {
                let _ = events.send(FetchEvent::Body(chunk.to_vec())).await;
            }
            Ok((received, None))
        }
        Body::File(download) => Ok((received, download.finish(events).await?)),
    }
}
