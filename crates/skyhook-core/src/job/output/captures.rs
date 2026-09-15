//! Actual file-backed captures, including those left behind by unfinished tools.
//!
//! Each pointer has its own atomic sidecar: concurrent writers cannot lose another
//! capture's registration. A reservation is not available until its data file exists.
use super::*;

mod stream;
pub(crate) use stream::{
    AsyncCapture, CaptureWriter, CompletedCapture, PendingCapture, TextCaptureField,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Text,
    Json,
    /// Transports may stream a capture before its final JSON type is known.
    #[default]
    Unknown,
}

#[derive(Deserialize, Serialize)]
struct Registration {
    field: String,
    kind: CaptureKind,
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

/// Reserve a capture's identity before its writer creates the file. Discovery
/// filters out uncreated reservations, and never parses capture bytes as JSON.
pub(crate) fn register_capture(
    directory: &Path,
    field: &str,
    kind: CaptureKind,
) -> std::io::Result<PathBuf> {
    validate_capture_field(field)?;
    std::fs::create_dir_all(directory)?;
    let path = field_file(directory, field);
    let mut metadata = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(
        &mut metadata,
        &Registration {
            field: field.into(),
            kind,
        },
    )?;
    metadata.flush()?;
    metadata
        .persist(registration_file(directory, field))
        .map_err(|error| error.error)?;
    Ok(path)
}

/// Descriptors describe raw captures, not a replacement structured result. In
/// particular, incomplete JSON is only safe to read through explicit byte paging.
#[derive(Serialize)]
pub(crate) struct CaptureDescriptor {
    pub(crate) field: String,
    pub(crate) kind: CaptureKind,
    pub(crate) complete: bool,
}

pub(crate) fn available_captures(
    directory: &Path,
    terminal: bool,
) -> Result<Vec<CaptureDescriptor>, ToolError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let document = if directory.join("document.json").exists() {
        Some(serde_json::from_reader::<_, Value>(BufReader::new(
            std::fs::File::open(directory.join("document.json"))?,
        ))?)
    } else {
        None
    };
    let complete = terminal
        && document
            .as_ref()
            .is_some_and(|document| document["capture_complete"].as_bool() == Some(true));
    // Merely having a value at a registered pointer does not mean that value
    // references the capture (for example, a producer may abandon it and return
    // null). Only saved external-field references can establish completion.
    let completed_fields: BTreeSet<String> = if complete {
        fields(directory)?.into_iter().collect()
    } else {
        BTreeSet::new()
    };
    let mut captures = BTreeMap::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("capture-field-") || !name.ends_with(".json") {
            continue;
        }
        let registration: Registration =
            serde_json::from_reader(BufReader::new(std::fs::File::open(entry.path())?))?;
        if !field_file(directory, &registration.field).is_file() {
            continue;
        }
        let field_complete = completed_fields.contains(&registration.field);
        captures.insert(
            registration.field.clone(),
            CaptureDescriptor {
                field: registration.field,
                kind: registration.kind,
                complete: field_complete,
            },
        );
    }
    Ok(captures.into_values().collect())
}

fn registration_file(directory: &Path, field: &str) -> PathBuf {
    let path = field_file(directory, field);
    directory.join(format!(
        "capture-{}.json",
        path.file_stem()
            .expect("capture filename")
            .to_string_lossy()
    ))
}
