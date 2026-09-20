//! Streamed or offloaded bytes at one output pointer, including those left behind by
//! unfinished tools.
//!
//! Each pointer is one `job_capture` row in the job's current generation, reserved
//! atomically, so concurrent producers cannot take over another capture.
use super::*;

mod stream;
pub(crate) use stream::{
    AsyncCapture, CaptureWriter, CompletedCapture, PendingCapture, TextCaptureField,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Text,
    Json,
    /// Transports may stream a capture before its final JSON type is known.
    #[default]
    Unknown,
}

impl CaptureKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn parse(kind: &str) -> Self {
        match kind {
            "text" => Self::Text,
            "json" => Self::Json,
            _ => Self::Unknown,
        }
    }
}

fn validate_capture_field(field: &str) -> std::io::Result<()> {
    let mut chars = field.chars();
    let valid_root = field.is_empty() || field.starts_with('/');
    let mut valid_escapes = true;
    while let Some(character) = chars.next() {
        if character == '~' && !matches!(chars.next(), Some('0' | '1')) {
            valid_escapes = false;
            break;
        }
    }
    if !valid_root || !valid_escapes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "capture field must be a JSON Pointer",
        ));
    }
    Ok(())
}

/// Descriptors describe raw captures, not a replacement structured result. In
/// particular, incomplete JSON is only safe to read through explicit byte paging.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct CaptureDescriptor {
    pub(crate) field: String,
    pub(crate) kind: CaptureKind,
    pub(crate) complete: bool,
    pub(crate) output: Option<Box<crate::job::JobView>>,
}

pub(crate) fn available_captures(saved: &Saved, terminal: bool) -> Vec<CaptureDescriptor> {
    // Merely having a value at a registered pointer does not mean that value
    // references the capture (for example, a producer may abandon it and return
    // null). Only the saved document's field references establish completion.
    let complete = terminal
        && saved
            .document
            .as_ref()
            .is_some_and(|document| document["capture_complete"].as_bool() == Some(true));
    saved
        .captures
        .values()
        .map(|capture| CaptureDescriptor {
            output: None,
            field: capture.pointer.clone(),
            kind: CaptureKind::parse(&capture.kind),
            complete: complete && capture.referenced,
        })
        .collect()
}
