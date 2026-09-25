//! Structured tool failure facts and their single capability-aware renderer.
//!
//! Sites remain facts until presentation: durable messages must not embed a target
//! alias that a later, less privileged reader is not allowed to discover.
use std::{
    io,
    path::{Path, PathBuf},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    execution::ExecutionLocation, identity::JobId, named_enum::named_enum, target::TargetRef,
    tool::policy::CapabilitySet,
};

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
    pub enum Operation {
        Execute = "execute",
        Validate = "validate",
        Authorize = "authorize",
        Connect = "connect",
        Inspect = "inspect",
        Canonicalize = "canonicalize",
        Read = "read",
        ReadDirectory = "read_directory",
        Create = "create",
        CreateDirectories = "create_directories",
        Write = "write",
        Copy = "copy",
        Remove = "remove",
        Rename = "rename",
        SetPermissions = "set_permissions",
        SyncFile = "sync_file",
        OpenDirectory = "open_directory",
        SyncDirectory = "sync_directory",
        Capture = "capture",
        CreateCapture = "create_capture",
        ReadCapture = "read_capture",
        WriteCapture = "write_capture",
        FinishCapture = "finish_capture",
        StoreImage = "store_image",
        Prepare = "prepare",
        Spawn = "spawn",
        Wait = "wait",
        Terminate = "terminate",
        Deserialize = "deserialize",
        Lookup = "lookup",
        Load = "load",
        Save = "save",
        Send = "send",
        Receive = "receive",
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
    pub enum Effects {
        Unknown = "unknown",
        NotStarted = "not_started",
        Started = "started",
        Unchanged = "unchanged",
        DestinationReplaced = "destination_replaced",
        PartialChange = "partial_change",
        OutputIncomplete = "output_incomplete",
        MayHaveExecuted = "may_have_executed",
    }
}

/// An explicitly selected safe subject, never arbitrary serialized arguments.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum Subject {
    None,
    Path(
        #[serde(with = "crate::execution::native_path")]
        #[schemars(with = "String")]
        PathBuf,
    ),
    WorkingDirectory(
        #[serde(with = "crate::execution::native_path")]
        #[schemars(with = "String")]
        PathBuf,
    ),
    StagingFile(
        #[serde(with = "crate::execution::native_path")]
        #[schemars(with = "String")]
        PathBuf,
    ),
    ParentDirectory(
        #[serde(with = "crate::execution::native_path")]
        #[schemars(with = "String")]
        PathBuf,
    ),
    DirectoryEntry(
        #[serde(with = "crate::execution::native_path")]
        #[schemars(with = "String")]
        PathBuf,
    ),
    Argument(String),
    Tool(String),
    Job(JobId),
    Process,
    Label(String),
}

impl Subject {
    pub fn path(path: impl AsRef<Path>) -> Self {
        Self::Path(path.as_ref().to_owned())
    }
    pub fn working_directory(path: impl AsRef<Path>) -> Self {
        Self::WorkingDirectory(path.as_ref().to_owned())
    }
    /// A JSON pointer into the tool arguments: the spelling schema validation reports.
    pub fn argument<S: AsRef<str>>(segments: impl IntoIterator<Item = S>) -> Self {
        Self::Argument(
            segments
                .into_iter()
                .map(|segment| format!("/{}", pointer_segment(segment.as_ref())))
                .collect(),
        )
    }
}

fn pointer_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// Invocation is resolved by the executor, not by the tool or renderer.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum FailureSite {
    Invocation,
    Host,
    Execution(ExecutionLocation),
}

impl FailureSite {
    pub fn bound(location: &ExecutionLocation, host: bool) -> Self {
        if host {
            Self::Host
        } else {
            Self::Execution(location.clone())
        }
    }
}

/// Rendering facts belong to the viewer, not the operation that produced the error.
#[derive(Clone, Copy)]
pub(crate) struct DiagnosticViewer<'a> {
    pub capabilities: &'a CapabilitySet,
    target: &'a TargetRef,
}

impl<'a> DiagnosticViewer<'a> {
    pub(crate) fn new(capabilities: &'a CapabilitySet, location: &'a ExecutionLocation) -> Self {
        Self {
            capabilities,
            target: &location.target,
        }
    }
}

/// Capability-only inspection is a host view; agent presentation supplies its location.
impl<'a> From<&'a CapabilitySet> for DiagnosticViewer<'a> {
    fn from(capabilities: &'a CapabilitySet) -> Self {
        Self {
            capabilities,
            target: &TargetRef::Root,
        }
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
    pub enum PathRole {
        Requested = "requested",
        Resolved = "resolved",
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct PathFact {
    pub role: PathRole,
    #[serde(with = "crate::execution::native_path")]
    #[schemars(with = "String")]
    pub path: PathBuf,
}

/// The resolved facts persistence, the wire and the renderer see.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DiagnosticContext {
    pub operation: Operation,
    pub subject: Subject,
    pub site: FailureSite,
    pub effects: Effects,
    pub paths: Vec<PathFact>,
}

/// The facts a failure site chose. Each boundary nearer the caller fills what
/// is still unset with [`PartialContext::or`]; nothing set is ever replaced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartialContext {
    operation: Option<(Operation, Subject)>,
    site: Option<FailureSite>,
    effects: Option<Effects>,
    paths: Vec<PathFact>,
}

impl PartialContext {
    pub fn new(operation: Operation, subject: Subject) -> Self {
        Self {
            operation: Some((operation, subject)),
            ..Self::default()
        }
    }
    #[must_use]
    pub fn path(mut self, role: PathRole, path: impl AsRef<Path>) -> Self {
        self.paths.push(PathFact {
            role,
            path: path.as_ref().to_owned(),
        });
        self
    }
    #[must_use]
    pub(crate) fn paths(mut self, paths: Vec<PathFact>) -> Self {
        self.paths = paths;
        self
    }
    /// Name what failed, keeping the stage already chosen; the neutral stage
    /// otherwise.
    #[must_use]
    pub fn subject(mut self, subject: Subject) -> Self {
        let operation = self
            .operation
            .map_or(Operation::Execute, |(operation, _)| operation);
        self.operation = Some((operation, subject));
        self
    }
    #[must_use]
    pub fn at(mut self, site: FailureSite) -> Self {
        self.site = Some(site);
        self
    }
    #[must_use]
    pub fn effects(mut self, effects: Effects) -> Self {
        self.effects = Some(effects);
        self
    }
    /// Fill the facts this context leaves unset from `fallback`.
    #[must_use]
    pub fn or(self, fallback: Self) -> Self {
        Self {
            operation: self.operation.or(fallback.operation),
            site: self.site.or(fallback.site),
            effects: self.effects.or(fallback.effects),
            paths: if self.paths.is_empty() {
                fallback.paths
            } else {
                self.paths
            },
        }
    }
    /// Unset facts resolve to the neutral spellings: a plain execution failure
    /// at the invocation whose effects are unknown.
    pub fn resolve(self) -> DiagnosticContext {
        let (operation, subject) = self
            .operation
            .unwrap_or((Operation::Execute, Subject::None));
        DiagnosticContext {
            operation,
            subject,
            site: self.site.unwrap_or(FailureSite::Invocation),
            effects: self.effects.unwrap_or(Effects::Unknown),
            paths: self.paths,
        }
    }
}

/// A resolved context, restored from persistence or the wire, has every fact set.
impl From<DiagnosticContext> for PartialContext {
    fn from(context: DiagnosticContext) -> Self {
        Self {
            operation: Some((context.operation, context.subject)),
            site: Some(context.site),
            effects: Some(context.effects),
            paths: context.paths,
        }
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
    pub enum IoKind {
        NotFound = "not_found",
        PermissionDenied = "permission_denied",
        AlreadyExists = "already_exists",
        InvalidInput = "invalid_input",
        InvalidData = "invalid_data",
        TimedOut = "timed_out",
        Interrupted = "interrupted",
        UnexpectedEof = "unexpected_eof",
        BrokenPipe = "broken_pipe",
        ConnectionRefused = "connection_refused",
        ConnectionReset = "connection_reset",
        ConnectionAborted = "connection_aborted",
        NotConnected = "not_connected",
        WouldBlock = "would_block",
        HostUnreachable = "host_unreachable",
        NetworkUnreachable = "network_unreachable",
        NetworkDown = "network_down",
        AddressInUse = "address_in_use",
        AddressNotAvailable = "address_not_available",
        WriteZero = "write_zero",
        Unsupported = "unsupported",
        OutOfMemory = "out_of_memory",
        IsADirectory = "is_a_directory",
        NotADirectory = "not_a_directory",
        DirectoryNotEmpty = "directory_not_empty",
        ReadOnlyFilesystem = "read_only_filesystem",
        StorageFull = "storage_full",
        Other = "other",
    }
}

impl From<io::ErrorKind> for IoKind {
    fn from(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            io::ErrorKind::InvalidInput => Self::InvalidInput,
            io::ErrorKind::InvalidData => Self::InvalidData,
            io::ErrorKind::TimedOut => Self::TimedOut,
            io::ErrorKind::Interrupted => Self::Interrupted,
            io::ErrorKind::UnexpectedEof => Self::UnexpectedEof,
            io::ErrorKind::BrokenPipe => Self::BrokenPipe,
            io::ErrorKind::ConnectionRefused => Self::ConnectionRefused,
            io::ErrorKind::ConnectionReset => Self::ConnectionReset,
            io::ErrorKind::ConnectionAborted => Self::ConnectionAborted,
            io::ErrorKind::NotConnected => Self::NotConnected,
            io::ErrorKind::HostUnreachable => Self::HostUnreachable,
            io::ErrorKind::NetworkUnreachable => Self::NetworkUnreachable,
            io::ErrorKind::NetworkDown => Self::NetworkDown,
            io::ErrorKind::AddrInUse => Self::AddressInUse,
            io::ErrorKind::AddrNotAvailable => Self::AddressNotAvailable,
            io::ErrorKind::WouldBlock => Self::WouldBlock,
            io::ErrorKind::WriteZero => Self::WriteZero,
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::OutOfMemory => Self::OutOfMemory,
            io::ErrorKind::IsADirectory => Self::IsADirectory,
            io::ErrorKind::NotADirectory => Self::NotADirectory,
            io::ErrorKind::DirectoryNotEmpty => Self::DirectoryNotEmpty,
            io::ErrorKind::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            io::ErrorKind::StorageFull => Self::StorageFull,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum Cause {
    Io {
        kind: IoKind,
        code: Option<i32>,
        detail: Option<String>,
    },
    InvalidArguments(String),
    Denied(String),
    Cancelled,
    Interrupted,
    InputClosed,
    Message(String),
    Json,
}

impl Cause {
    pub fn io(error: &io::Error) -> Self {
        Self::Io {
            kind: error.kind().into(),
            code: error.raw_os_error(),
            detail: error.get_ref().map(|reason| safe_text(&reason.to_string())),
        }
    }

    /// Opaque subsystem messages may contain arguments, credentials or payloads.
    /// Redaction changes only detail, never classification or the originating OS code.
    pub(crate) fn redact_io_detail(&mut self) {
        if let Self::Io { detail, .. } = self {
            *detail = None;
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct Diagnostic {
    pub context: DiagnosticContext,
    pub cause: Cause,
}

/// A failure's facts before its boundaries have filled them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartialDiagnostic {
    pub context: PartialContext,
    pub cause: Cause,
}

impl PartialDiagnostic {
    pub const fn new(context: PartialContext, cause: Cause) -> Self {
        Self { context, cause }
    }
    pub const fn is_denial(&self) -> bool {
        matches!(self.cause, Cause::Denied(_))
    }
    #[must_use]
    pub fn or(mut self, fallback: PartialContext) -> Self {
        self.context = self.context.or(fallback);
        self
    }
    pub fn resolve(self) -> Diagnostic {
        Diagnostic::new(self.context.resolve(), self.cause)
    }
    pub(crate) fn session(error: &crate::session::SessionError) -> Self {
        use crate::session::SessionError::*;
        use Cause::Message;
        let cause = match error {
            Io(error) => {
                let mut cause = Cause::io(error);
                cause.redact_io_detail();
                cause
            }
            AppendIndeterminate(_) => {
                Message("session append completion is unknown; recovery is required".into())
            }
            AppendUnavailable(_) => {
                Message("session append rejected; writer recovery is required".into())
            }
            Closed => Message("session writer is closed".into()),
            Database(_) => Message("session database operation failed".into()),
            TemplateHistory => {
                Message("model request template must not contain conversation history".into())
            }
            ModelRequestReplay { sequence, reason } => Message(format!(
                "cannot reconstruct model request at sequence {sequence}: {reason}"
            )),
            Random(_) => Message("session identifier generation failed".into()),
            IdCollisions => Message("could not allocate a unique session identifier".into()),
            AlreadyOpen(session) => Message(format!("session {session} is already open")),
            UnsupportedVersion(version) => {
                Message(format!("unsupported session version {version}"))
            }
            WrongSession => Message("event belongs to another session".into()),
            BlobHashMismatch(_) => Message("stored blob failed its content hash check".into()),
        };
        Self::new(PartialContext::default().at(FailureSite::Host), cause)
    }
}

impl From<Diagnostic> for PartialDiagnostic {
    fn from(diagnostic: Diagnostic) -> Self {
        Self::new(diagnostic.context.into(), diagnostic.cause)
    }
}

impl Diagnostic {
    pub const fn new(context: DiagnosticContext, cause: Cause) -> Self {
        Self { context, cause }
    }
    pub const fn is_denial(&self) -> bool {
        matches!(self.cause, Cause::Denied(_))
    }
    /// A worker is authoritative about its path, never about a session target alias.
    /// All worker execution sites map to the host's trusted connection destination.
    pub fn bind_worker(&mut self, location: &ExecutionLocation) {
        self.context.site = FailureSite::Execution(location.clone());
    }
    /// Render for a host viewer. Agent-facing presentation also supplies its target.
    pub fn render(&self, capabilities: &CapabilitySet) -> String {
        self.render_for(capabilities.into())
    }

    pub(crate) fn render_for(&self, viewer: DiagnosticViewer<'_>) -> String {
        let capabilities = viewer.capabilities;
        let mut text = self.context.operation.as_str().replace('_', " ");
        text.push_str(&match &self.context.subject {
            Subject::None => String::new(),
            Subject::Path(path) => format!(" path {}", quoted_path(path)),
            Subject::WorkingDirectory(path) => format!(" working directory {}", quoted_path(path)),
            Subject::StagingFile(path) => format!(" staging file for {}", quoted_path(path)),
            Subject::ParentDirectory(path) => format!(" parent directory {}", quoted_path(path)),
            Subject::DirectoryEntry(path) => format!(" directory entry {}", quoted_path(path)),
            Subject::Argument(path) => format!(" argument {}", quoted(path)),
            Subject::Tool(tool) => format!(" tool {}", quoted(tool)),
            Subject::Job(job) => format!(" job {job}"),
            Subject::Process => " process".into(),
            Subject::Label(label) => format!(" {}", safe_text(label)),
        });
        for path in &self.context.paths {
            text.push_str(&format!(
                " ({} {})",
                path.role.as_str(),
                quoted_path(&path.path)
            ));
        }
        text.push_str(" failed");
        match &self.context.site {
            FailureSite::Invocation => {}
            FailureSite::Host => {
                if *viewer.target != TargetRef::Root {
                    text.push_str(" on session host");
                }
            }
            FailureSite::Execution(location) => {
                match capabilities.visible_target(&location.target) {
                    Some(target) => {
                        text.push_str(&format!(" on target {}", quoted(target.as_str())))
                    }
                    None => text.push_str(" in execution workspace"),
                }
                // The workspace is relevant even for an absolute requested path: cwd
                // can fail before that requested operation starts.
                text.push_str(&format!(
                    " (workspace {})",
                    quoted_path(&location.workspace)
                ));
            }
        }
        text.push_str(": ");
        match &self.cause {
            Cause::Io { kind, code, detail } => {
                text.push_str(&kind.as_str().replace('_', " "));
                if let Some(code) = code {
                    text.push_str(&format!(" (OS error {code})"));
                }
                if let Some(detail) = detail {
                    text.push_str(&format!(": {}", safe_text(detail)));
                }
            }
            Cause::InvalidArguments(message) | Cause::Message(message) => {
                text.push_str(&safe_text(message))
            }
            Cause::Denied(reason) => {
                text.push_str("permission denied");
                if !reason.is_empty() {
                    text.push_str(&format!(" ({})", safe_text(reason)));
                }
            }
            Cause::Cancelled => text.push_str("operation cancelled"),
            Cause::Interrupted => text.push_str("operation interrupted; completion is unknown"),
            Cause::InputClosed => text.push_str("tool input channel is closed"),
            Cause::Json => text.push_str("invalid JSON data"),
        }
        if self.context.operation == Operation::Remove
            && matches!(
                self.cause,
                Cause::Io {
                    kind: IoKind::DirectoryNotEmpty,
                    ..
                }
            )
        {
            text.push_str(
                "; use recursive removal only if all directory contents should be deleted",
            );
        }
        text.push_str(match self.context.effects {
            Effects::Unknown => "",
            Effects::NotStarted => "; operation did not start",
            Effects::Started => "; operation started",
            Effects::Unchanged => "; destination unchanged",
            Effects::DestinationReplaced => "; destination replaced; durability is uncertain",
            Effects::PartialChange => "; partial changes may remain",
            Effects::OutputIncomplete => "; output incomplete",
            Effects::MayHaveExecuted => "; request may have executed; completion is unknown",
        });
        text
    }
}

/// Remove an opaque I/O source message at a boundary that can contain secrets,
/// retaining its typed kind and OS code without interpreting the message.
pub(crate) fn opaque_io(error: impl std::borrow::Borrow<io::Error>) -> io::Error {
    let error = error.borrow();
    match error.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code),
        None => error.kind().into(),
    }
}

/// Show control characters as escapes, so untrusted names and messages can
/// neither drive a terminal nor fake line structure.
pub fn escape_controls(value: impl std::fmt::Display) -> String {
    let mut output = String::new();
    for character in value.to_string().chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

/// Bound externally opaque text and remove terminal controls and URL credentials.
/// This is presentation sanitization, never failure classification.
pub(crate) fn safe_text(value: &str) -> String {
    let mut text = String::new();
    for word in value.split_whitespace() {
        if !text.is_empty() {
            text.push(' ');
        }
        let mut word = word
            .chars()
            .filter(|ch| !ch.is_control())
            .collect::<String>();
        if let Some(scheme) = word.find("://") {
            let authority = scheme + 3;
            let end = word[authority..]
                .find(['/', '?', '#'])
                .map_or(word.len(), |i| authority + i);
            if let Some(at) = word[authority..end].rfind('@') {
                word.replace_range(authority..authority + at + 1, "[redacted]@");
            }
        }
        text.push_str(&word);
        if text.chars().count() > 512 {
            text = text.chars().take(512).collect();
            text.push('…');
            break;
        }
    }
    text
}

fn quoted_path(path: &Path) -> String {
    match path.to_str() {
        Some(path) => quoted(path),
        None => {
            let bytes = path.as_os_str().as_encoded_bytes();
            let escaped = bytes
                .iter()
                .take(512)
                .flat_map(|byte| byte.escape_ascii())
                .map(char::from)
                .collect::<String>();
            format!(
                "native path bytes `{}`{}",
                escaped.replace('`', "\\`"),
                if bytes.len() > 512 { "…" } else { "" }
            )
        }
    }
}

fn quoted(value: &str) -> String {
    let escaped = value
        .chars()
        .take(512)
        .flat_map(char::escape_debug)
        .collect::<String>();
    let suffix = if value.chars().count() > 512 {
        "…"
    } else {
        ""
    };
    format!("`{}`{suffix}", escaped.replace('`', "\\`"))
}

/// Deserialize once at the argument boundary. Only rejected inputs need schema
/// diagnostics; serde's errors may echo credentials or user-provided content.
pub(crate) fn deserialize_arguments<T: serde::de::DeserializeOwned + JsonSchema>(
    value: &serde_json::Value,
) -> Result<T, super::AdmissionError> {
    serde_path_to_error::deserialize(value).map_err(|error| {
        let schema =
            serde_json::to_value(schemars::schema_for!(T)).expect("generated schema serializes");
        let (path, expectation) = jsonschema::options()
            .with_retriever(NoExternalArgumentSchemas)
            .build(&schema)
            .ok()
            .and_then(|validator| {
                validator
                    .validate(value)
                    .err()
                    .map(|error| schema_argument_failure(&schema, value, &error))
            })
            .unwrap_or_else(|| {
                // Custom deserializers may reject values the schema accepts.
                // Retain the declared expectation without reading serde's Display.
                let (path, node) = argument_location(
                    &schema,
                    error.path().iter().map(|segment| match segment {
                        serde_path_to_error::Segment::Map { key } => {
                            ArgumentPathSegment::Property(key)
                        }
                        serde_path_to_error::Segment::Seq { index } => {
                            ArgumentPathSegment::Index(*index)
                        }
                        _ => ArgumentPathSegment::Unknown,
                    }),
                );
                let expectation = node
                    .and_then(argument_expectation)
                    .unwrap_or_else(|| "invalid argument value".to_owned());
                (path, expectation)
            });
        super::AdmissionError::invalid_arguments(expectation)
            .operation(Operation::Deserialize, Subject::Argument(path))
    })
}

/// Schema diagnostics must never retrieve filesystem or network resources, even
/// when dependency feature unification enables a default external resolver.
pub(crate) struct NoExternalArgumentSchemas;

impl jsonschema::Retrieve for NoExternalArgumentSchemas {
    fn retrieve(
        &self,
        _uri: &jsonschema::Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external argument schema references are not permitted".into())
    }
}

/// Keep only declared expectations from a typed validation failure. Neither the
/// rejected instance nor dynamic property names from its error kind are rendered.
pub(crate) fn schema_argument_failure(
    schema: &serde_json::Value,
    input: &serde_json::Value,
    error: &jsonschema::ValidationError<'_>,
) -> (String, String) {
    // JSON pointers cannot distinguish numeric map keys from array indices.
    // Decode against the instance, preserving empty property names too.
    let components: Vec<_> = error
        .instance_path()
        .as_str()
        .split('/')
        .skip(1)
        .map(|component| component.replace("~1", "/").replace("~0", "~"))
        .collect();
    let mut node = input;
    let segments = components.iter().map(|component| {
        let segment = match node {
            serde_json::Value::Object(_) => ArgumentPathSegment::Property(component),
            serde_json::Value::Array(_) => component
                .parse::<usize>()
                .map_or(ArgumentPathSegment::Unknown, ArgumentPathSegment::Index),
            _ => ArgumentPathSegment::Unknown,
        };
        node = match segment {
            ArgumentPathSegment::Property(key) => node.get(key),
            ArgumentPathSegment::Index(index) => node.get(index),
            ArgumentPathSegment::Unknown => None,
        }
        .unwrap_or(&serde_json::Value::Null);
        segment
    });
    (
        safe_argument_path(schema, segments),
        validation_expectation(error.kind(), 0),
    )
}

fn validation_expectation(kind: &jsonschema::error::ValidationErrorKind, depth: usize) -> String {
    use jsonschema::error::{TypeKind, ValidationErrorKind as Kind};
    match kind {
        Kind::Required { property } => {
            format!("missing required field {}", quoted_schema(property))
        }
        Kind::Type { kind } => match kind {
            TypeKind::Single(kind) => format!("expected {}", argument_type(*kind)),
            TypeKind::Multiple(kinds) => format!(
                "expected {}",
                kinds
                    .iter()
                    .map(argument_type)
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        },
        Kind::Enum { options } => format!("expected one of {}", quoted_schema(options)),
        Kind::Constant { expected_value } => {
            format!("expected constant {}", quoted_schema(expected_value))
        }
        Kind::Minimum { limit } => format!("minimum {limit}"),
        Kind::Maximum { limit } => format!("maximum {limit}"),
        Kind::ExclusiveMinimum { limit } => format!("must be greater than {limit}"),
        Kind::ExclusiveMaximum { limit } => format!("must be less than {limit}"),
        Kind::MultipleOf { multiple_of } => format!("must be a multiple of {multiple_of}"),
        Kind::MinLength { limit } => format!("minimum length {limit}"),
        Kind::MaxLength { limit } => format!("maximum length {limit}"),
        Kind::MinItems { limit } => format!("minimum items {limit}"),
        Kind::MaxItems { limit } => format!("maximum items {limit}"),
        Kind::MinProperties { limit } => format!("minimum properties {limit}"),
        Kind::MaxProperties { limit } => format!("maximum properties {limit}"),
        Kind::AdditionalItems { limit } => format!("maximum items {limit}"),
        Kind::UniqueItems => "array items must be unique".into(),
        Kind::Contains => "array must contain an item matching the declared schema".into(),
        Kind::Pattern { pattern } => format!("must match pattern {}", quoted(pattern)),
        Kind::Format { format } => format!("expected format {}", quoted(format)),
        Kind::ContentEncoding { content_encoding } => {
            format!("expected content encoding {}", quoted(content_encoding))
        }
        Kind::ContentMediaType { content_media_type } => {
            format!("expected content media type {}", quoted(content_media_type))
        }
        Kind::AdditionalProperties { .. } | Kind::UnevaluatedProperties { .. } => {
            "additional properties are not allowed".into()
        }
        Kind::UnevaluatedItems { .. } => "additional array items are not allowed".into(),
        Kind::AnyOf { context } | Kind::OneOfNotValid { context } if depth < 3 => {
            let alternatives = context
                .iter()
                .take(4)
                .map(|errors| {
                    errors
                        .iter()
                        .take(2)
                        .map(|error| validation_expectation(error.kind(), depth + 1))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .collect::<Vec<_>>()
                .join(" / ");
            let requirement = if matches!(kind, Kind::OneOfNotValid { .. }) {
                "exactly one"
            } else {
                "at least one"
            };
            format!("must match {requirement} schema alternative: {alternatives}")
        }
        Kind::AnyOf { .. } => "must match a schema alternative".into(),
        Kind::OneOfNotValid { .. } | Kind::OneOfMultipleValid { .. } => {
            "must match exactly one schema alternative".into()
        }
        Kind::PropertyNames { error } if depth < 3 => format!(
            "invalid property name: {}",
            validation_expectation(error.kind(), depth + 1)
        ),
        Kind::PropertyNames { .. } => "property names must match the declared schema".into(),
        Kind::Not { .. } | Kind::FalseSchema => "value is not allowed by the schema".into(),
        Kind::FromUtf8 { .. } => "content must decode to UTF-8".into(),
        Kind::BacktrackLimitExceeded { .. }
        | Kind::RegexEngineFailure { .. }
        | Kind::Referencing(_)
        | Kind::Custom { .. } => "input could not be validated against the schema".into(),
    }
}

fn quoted_schema(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => quoted(value),
        value => quoted(&value.to_string()),
    }
}

fn argument_type(kind: jsonschema::JsonType) -> &'static str {
    use jsonschema::JsonType;
    match kind {
        JsonType::Integer => "an integer",
        JsonType::Number => "a number",
        JsonType::String => "a string",
        JsonType::Boolean => "a boolean",
        JsonType::Array => "an array",
        JsonType::Object => "an object",
        JsonType::Null => "null",
    }
}

/// A parsed argument location, independent of the native or MCP validator.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ArgumentPathSegment<'a> {
    Property(&'a str),
    Index(usize),
    Unknown,
}

/// Render a JSON pointer using only schema-declared names and indices. Dynamic
/// keys become `*`, but their value schemas can still declare safe descendants.
/// Common local references are followed; unsupported schemas hide names. Root is "".
pub(crate) fn safe_argument_path<'a>(
    schema: &serde_json::Value,
    path: impl IntoIterator<Item = ArgumentPathSegment<'a>>,
) -> String {
    argument_location(schema, path).0
}

fn argument_location<'schema, 'path>(
    schema: &'schema serde_json::Value,
    path: impl IntoIterator<Item = ArgumentPathSegment<'path>>,
) -> (String, Option<&'schema serde_json::Value>) {
    let resolver = crate::json_schema::Resolver::new(schema);
    let mut node = Some(resolver.root());
    let mut rendered = String::new();
    // Follow references without selecting alternative branches.
    for segment in path {
        node = node.and_then(|node| resolver.resolve(node));
        rendered.push('/');
        node = match segment {
            ArgumentPathSegment::Property(key) => {
                let property = node.and_then(|node| node.schema.get("properties")?.get(key));
                if property.is_some() {
                    rendered.push_str(&pointer_segment(key));
                } else {
                    rendered.push('*');
                }
                node.and_then(|node| {
                    Some(node.child(property.or_else(|| node.schema.get("additionalProperties"))?))
                })
            }
            ArgumentPathSegment::Index(index) => {
                rendered.push_str(&index.to_string());
                node.and_then(|node| Some(node.child(node.schema.get("items")?)))
            }
            ArgumentPathSegment::Unknown => {
                rendered.push('*');
                None
            }
        };
    }
    let leaf = node.and_then(|node| resolver.resolve(node));
    (rendered, leaf.map(|node| node.schema))
}

/// Read expectations from the same trusted leaf used for path privacy, never
/// serde's Display (which embeds the rejected value).
fn argument_expectation(node: &serde_json::Value) -> Option<String> {
    let kind = node.get("type")?.as_str()?.parse().ok()?;
    let mut expectation = format!("expected {}", argument_type(kind));
    for (key, label) in [
        ("minimum", "minimum"),
        ("maximum", "maximum"),
        ("minLength", "minimum length"),
        ("maxLength", "maximum length"),
        ("minItems", "minimum items"),
        ("maxItems", "maximum items"),
    ] {
        if let Some(value) = node.get(key).and_then(serde_json::Value::as_number) {
            expectation.push_str(&format!("; {label} {value}"));
        }
    }
    Some(expectation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::invocation::OperationError;
    use serde_json::json;
    #[test]
    fn rendering_preserves_paths_but_hides_targets_and_opaque_credentials() {
        let diagnostic = Diagnostic::new(
            PartialContext::new(Operation::Read, Subject::path("two  spaces\n\u{1b}[2J"))
                .at(FailureSite::Execution(ExecutionLocation::named(
                    "bastion".parse().unwrap(),
                    "/work".into(),
                )))
                .resolve(),
            Cause::Message("failed https://user:password@example.test/path\n\u{1b}oops".into()),
        );
        let text = diagnostic.render(&CapabilitySet::default());
        assert!(text.contains("two  spaces\\n\\u{1b}[2J"));
        for secret in ["bastion", "password", "\u{1b}"] {
            assert!(!text.contains(secret));
        }
        let mut host = diagnostic.clone();
        host.context.site = FailureSite::Host;
        let capabilities = CapabilitySet::default();
        let local = ExecutionLocation::root("/local".into());
        let remote = ExecutionLocation::named("worker".parse().unwrap(), "/remote".into());
        assert!(
            !host
                .render_for(DiagnosticViewer::new(&capabilities, &local))
                .contains("on session host")
        );
        assert!(
            host.render_for(DiagnosticViewer::new(&capabilities, &remote))
                .contains("on session host")
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let mut diagnostic = diagnostic;
            let path = std::ffi::OsString::from_vec(vec![b'/', 0xff]);
            diagnostic.context.subject = Subject::path(PathBuf::from(path));
            let decoded: Diagnostic =
                serde_json::from_value(serde_json::to_value(&diagnostic).unwrap()).unwrap();
            assert_eq!(decoded, diagnostic);
            let text = decoded.render(&CapabilitySet::default());
            assert!(text.contains("\\xff"));
            assert!(!text.contains('�'));
        }
    }
    #[test]
    fn native_arguments_report_declared_paths_and_types_without_user_data() {
        #[derive(Debug, Deserialize, JsonSchema)]
        #[allow(dead_code)]
        struct Input {
            entries: Vec<std::collections::HashMap<String, Entry>>,
        }
        #[derive(Debug, Deserialize, JsonSchema)]
        #[allow(dead_code)]
        struct Entry {
            #[serde(rename = "a~/b")]
            count: u32,
            mode: Option<Mode>,
        }
        #[derive(Debug, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        enum Mode {
            Fast,
            Safe,
        }
        for (value, path, expectations) in [
            (
                json!({"entries": [{"secret-key": {"a~/b": "secret-value"}}]}),
                "/entries/0/*/a~0~1b",
                vec!["expected an integer"],
            ),
            (json!({}), "", vec!["missing required field `entries`"]),
            (
                json!({"entries": [{"secret-key": {}}]}),
                "/entries/0/*",
                vec!["missing required field `a~/b`"],
            ),
            (
                json!({"entries": [{"secret-key": {"a~/b": -1}}]}),
                "/entries/0/*/a~0~1b",
                vec!["minimum 0"],
            ),
            (
                json!({"entries": [{"secret-key": {"a~/b": 1, "mode": "secret-value"}}]}),
                "/entries/0/*/mode",
                vec!["expected one of", "fast", "safe"],
            ),
        ] {
            let error = deserialize_arguments::<Input>(&value).unwrap_err();
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.subject, Subject::Argument(path.into()));
            let text = diagnostic.render(&CapabilitySet::default());
            for expectation in expectations {
                assert!(text.contains(expectation), "{text}");
            }
            assert!(
                !serde_json::to_string(&diagnostic)
                    .unwrap()
                    .contains("secret")
            );
        }
    }

    #[test]
    fn argument_locations_preserve_declarations_and_hide_unknown_schema_paths() {
        use ArgumentPathSegment::{Index as I, Property as P, Unknown as U};
        let schema = json!({
            "$defs": {"entry/name": {"properties": {
                "value": {"type": "integer", "minimum": 1},
                "next": {"$ref": "#/$defs/entry~1name"}
            }}},
            "properties": {
                "map": {"additionalProperties": {"$ref": "#/$defs/entry~1name"}},
                "array": {"items": {"$ref": "#/$defs/entry~1name"}},
                "~a/b": {"properties": {"": {"properties": {"007": {}}}}},
                "cycle": {"$ref": "#/properties/cycle"},
                "external": {"$ref": "https://example.test/schema"},
                "pattern": {"patternProperties": {".*": {"properties": {"value": {}}}}},
                "choice": {"anyOf": [{"type": "integer"}, {"type": "string"}]}
            }
        });
        let integer = Some("expected an integer; minimum 1");
        for (path, expected, expectation) in [
            (vec![], "", None),
            (
                vec![P("map"), P("secret"), P("value")],
                "/map/*/value",
                integer,
            ),
            (
                vec![P("array"), I(2), P("next"), P("value")],
                "/array/2/next/value",
                integer,
            ),
            (vec![P("~a/b"), P(""), P("007")], "/~0a~1b//007", None),
            (vec![P("map"), P("123456"), P("unknown")], "/map/*/*", None),
            (vec![P("unknown"), P("map")], "/*/*", None),
            (vec![U, P("map")], "/*/*", None),
            (vec![P("cycle"), P("value")], "/cycle/*", None),
            (vec![P("external"), P("value")], "/external/*", None),
            (
                vec![P("pattern"), P("secret"), P("value")],
                "/pattern/*/*",
                None,
            ),
            (vec![P("choice")], "/choice", None),
        ] {
            assert_eq!(safe_argument_path(&schema, path.clone()), expected);
            let (_, node) = argument_location(&schema, path);
            assert_eq!(node.and_then(argument_expectation).as_deref(), expectation);
        }
    }

    #[test]
    fn normalization_retains_facts_and_output_but_not_local_provenance() {
        let context =
            PartialContext::new(Operation::Deserialize, Subject::argument(["items", "0"]))
                .at(FailureSite::Host)
                .effects(Effects::OutputIncomplete);
        let wire = Diagnostic::new(
            PartialContext::default().resolve(),
            Cause::Io {
                kind: IoKind::PermissionDenied,
                code: Some(2),
                detail: Some("remote reason".into()),
            },
        );
        let native = serde_json::from_str::<serde_json::Value>("{private-payload").unwrap_err();
        for (error, cause, source) in [
            (OperationError::from(native), Cause::Json, false),
            (
                OperationError::source_filesystem_io(io::Error::other("filesystem reason")),
                Cause::io(&io::Error::other("filesystem reason")),
                true,
            ),
            (
                OperationError::from_diagnostic(wire.clone(), None),
                wire.cause,
                false,
            ),
        ] {
            let error = error
                .with_result(1_u8)
                .context(context.clone())
                .with_result(7)
                .try_map_output(|value| Ok::<_, ()>(u16::from(value)))
                .unwrap();
            assert_eq!(error.is_source_filesystem_io(), source);
            let (diagnostic, output) = error.into_parts();
            assert_eq!(diagnostic.cause, cause);
            assert_eq!(diagnostic.context, context.clone().resolve());
            assert_eq!(output, Some(7_u16));
            let text = diagnostic.render(&CapabilitySet::default());
            assert!(!text.contains("private-payload"));
            // Every fact of a restored diagnostic counts as chosen.
            let decoded =
                serde_json::from_value(serde_json::to_value(&diagnostic).unwrap()).unwrap();
            let restored = OperationError::from_diagnostic(decoded, output).or(
                PartialContext::new(Operation::Copy, Subject::path("fallback")),
            );
            assert!(!restored.is_source_filesystem_io());
            assert_eq!(restored.into_parts(), (diagnostic, output));
        }
    }

    #[test]
    fn session_normalization_preserves_classification_and_redacts_opaque_details() {
        use crate::{
            identity::{EventId, SessionId},
            job::JobError,
            session::{AppendIdentity, AppendRecovery, DbError, SessionError},
            tool::{ToolError, ToolOutput, executor::ExecutionError},
        };
        use std::sync::Arc;

        const SECRET: &str = "private-session-payload";
        fn recovery() -> AppendRecovery {
            AppendRecovery {
                identity: AppendIdentity {
                    event: EventId::from_bytes([1; 16]),
                    session: SessionId::from_bytes([2; 16]),
                    sequence: 3.into(),
                },
                reason: SECRET.into(),
            }
        }
        for (error, message) in [
            (
                SessionError::Database(DbError::Corrupt(SECRET.into())),
                "database operation failed",
            ),
            (SessionError::Closed, "writer is closed"),
            (
                SessionError::BlobHashMismatch(SECRET.into()),
                "content hash check",
            ),
            (
                SessionError::AppendIndeterminate(recovery()),
                "completion is unknown",
            ),
            (
                SessionError::AppendUnavailable(recovery()),
                "append rejected",
            ),
        ] {
            let diagnostic = PartialDiagnostic::session(&error).resolve();
            assert!(matches!(&diagnostic.cause, Cause::Message(text) if text.contains(message)));
            assert_eq!(diagnostic.context.site, FailureSite::Host);
            let text = diagnostic.render(&CapabilitySet::default());
            assert!(!text.contains(SECRET));
        }
        let fallback = PartialContext::new(
            Operation::WriteCapture,
            Subject::Job(JobId::new(7).unwrap()),
        )
        .effects(Effects::OutputIncomplete);
        let cases: [fn() -> io::Error; 2] = [
            || io::Error::other(SECRET),
            || io::Error::from_raw_os_error(13),
        ];
        for make in cases {
            let mut expected = Cause::io(&make());
            expected.redact_io_detail();
            assert_eq!(Cause::io(&opaque_io(make())), expected);
            assert_eq!(
                ToolError::io(make()).opaque_io().diagnostic().cause,
                expected
            );
            for error in [
                ToolError::from(SessionError::Io(make())),
                ToolError::from(Arc::new(SessionError::Io(make()))),
                ExecutionError::from(JobError::from(SessionError::Io(make()))).into_tool_error(),
            ] {
                let (diagnostic, output) = error
                    .with_result(ToolOutput::new(json!({"partial": true})))
                    .or(fallback.clone())
                    .into_parts();
                assert_eq!(diagnostic.cause, expected);
                assert_eq!(
                    diagnostic.context,
                    fallback.clone().at(FailureSite::Host).resolve()
                );
                assert_eq!(output.unwrap().value, json!({"partial": true}));
            }
        }
    }
}
