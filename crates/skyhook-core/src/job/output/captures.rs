//! Actual file-backed captures, including those left behind by unfinished tools.
//!
//! Each pointer has its own atomic sidecar: concurrent writers cannot lose another
//! capture's registration. A reservation is not available until its data file exists.
use super::*;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaptureKind {
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

/// Reserve a capture's identity before its writer creates the file. Discovery
/// filters out uncreated reservations, and never parses capture bytes as JSON.
pub(crate) fn register_capture(
    directory: &Path,
    field: &str,
    kind: CaptureKind,
) -> std::io::Result<PathBuf> {
    if !field.is_empty() && !field.starts_with('/') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "capture field must be a JSON Pointer",
        ));
    }
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
    // Older sessions saved pointer identities in fields.json but had no sidecars.
    // Recover types from the compact tree, never from potentially partial bytes.
    if directory.join("fields.json").exists() {
        for field in fields(directory)? {
            if !field_file(directory, &field).is_file() {
                continue;
            }
            let kind = match document.as_ref().and_then(|value| value.pointer(&field)) {
                Some(Value::String(_)) => CaptureKind::Text,
                Some(Value::Object(_) | Value::Array(_)) => CaptureKind::Json,
                _ => CaptureKind::Unknown,
            };
            captures.insert(
                field.clone(),
                CaptureDescriptor {
                    complete: completed_fields.contains(&field),
                    field,
                    kind,
                },
            );
        }
    }
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

/// Migrate sidecar-less captures before finish replaces the old fields index.
/// The small identity fallback is only for unfinished legacy built-ins; current
/// producers must register arbitrary captures explicitly.
pub(crate) fn recover_legacy_captures(
    directory: &Path,
    unfinished_tool: Option<&str>,
) -> Result<(), ToolError> {
    for capture in available_captures(directory, false)? {
        if !registration_file(directory, &capture.field).exists() {
            register_capture(directory, &capture.field, capture.kind)?;
        }
    }
    let known: &[&str] = match unfinished_tool {
        Some("exec" | "shell") => &["/result/stdout", "/result/stderr"],
        Some("script") => &["/result/console"],
        _ => &[],
    };
    for field in known {
        if field_file(directory, field).is_file() && !registration_file(directory, field).exists() {
            register_capture(directory, field, CaptureKind::Text)?;
        }
    }
    Ok(())
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
