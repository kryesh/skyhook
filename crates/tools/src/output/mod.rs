//! A tool call's output: its value and the evidence its producer owns, the captures its
//! fields stream into, and how presentation shows its fields.

pub(crate) mod capture;
pub mod json;
mod pointer;
mod presentation;

use serde::Serialize;
use serde_json::Value;
use skyhook_media::ImageRef;

pub use capture::{
    Abandon, Abandonment, AsyncProducer, CaptureEvent, CaptureId, CaptureKind, CaptureTarget,
    EventCapture, OutputContext, OutputEvent, OutputSink, OutputWriter, PendingProducer, Producer,
};
pub(crate) use capture::{AsyncOutput, PendingOutput};
pub use pointer::{FieldPointer, InvalidFieldPointer, TextCaptureField};
pub use presentation::{FieldPresentation, JsonText, PREVIEW, Presented, complete, ends};

/// Producer completion selects an output.
#[derive(Clone, Debug)]
pub struct FinishedOutput {
    id: CaptureId,
    file: Option<CaptureFile>,
}

impl FinishedOutput {
    pub fn id(&self) -> CaptureId {
        self.id
    }

    /// The file that holds all of the capture's bytes, when it holds only their first.
    pub fn file(&self) -> Option<&CaptureFile> {
        self.file.as_ref()
    }

    /// This capture as the first bytes of `file`.
    #[must_use]
    pub fn in_file(mut self, file: CaptureFile) -> Self {
        self.file = Some(file);
        self
    }
}

/// A file on the target a call ran on that holds all of a field's bytes, of which
/// the field's capture holds only the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureFile {
    /// A whole stream, as the target wrote it in its spill directory.
    Written(skyhook_worker::op::SpilledStream),
    /// A file the call named, holding `extent` when it read it, and read as it is
    /// whenever it is read again.
    Named {
        path: skyhook_worker::op::TargetPath,
        extent: skyhook_worker::op::Extent,
    },
}

impl CaptureFile {
    pub fn path(&self) -> &skyhook_worker::op::TargetPath {
        match self {
            Self::Written(stream) => &stream.path,
            Self::Named { path, .. } => path,
        }
    }

    /// What the file held as its target counted it: as it wrote it, or as the call
    /// read it.
    pub fn extent(&self) -> skyhook_worker::op::Extent {
        match self {
            Self::Written(stream) => stream.extent,
            Self::Named { extent, .. } => *extent,
        }
    }
}

/// A handler's value and the evidence its producer owns: the captures `C` its
/// fields stream into, its images, whether its streams ran to their end, and the
/// presentations its fields carry beyond those its schema declares.
#[derive(Clone, Debug)]
pub struct Output<C> {
    pub value: Value,
    pub images: Vec<ImageRef>,
    pub captures: Vec<C>,
    pub streams: StreamEnd,
    pub presented: Presented,
    /// The files it reports that its target created in its spill directory.
    pub spilled: Vec<skyhook_worker::fs::spill::SpillFile>,
}

impl<C> Default for Output<C> {
    fn default() -> Self {
        Self::new(Value::Null)
    }
}

/// Local production, before the session host imports its captures.
pub type ProducedOutput = Output<FinishedOutput>;

impl<C> Output<C> {
    /// A native value serialized as the output.
    pub fn serialized(value: impl Serialize) -> Result<Self, serde_json::Error> {
        serde_json::to_value(value).map(Self::new)
    }

    #[must_use]
    pub const fn new(value: Value) -> Self {
        Self {
            value,
            images: Vec::new(),
            captures: Vec::new(),
            streams: StreamEnd::Finished,
            presented: Presented::new(),
            spilled: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_captures(mut self, captures: Vec<C>) -> Self {
        self.captures = captures;
        self
    }

    #[must_use]
    pub fn with_images(mut self, images: Vec<ImageRef>) -> Self {
        self.images = images;
        self
    }

    #[must_use]
    pub fn with_presented(mut self, presented: Presented) -> Self {
        self.presented = presented;
        self
    }
}

/// Whether a producer's streams ran to their end; a timeout cuts them short.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEnd {
    #[default]
    Finished,
    Cut,
}
