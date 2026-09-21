//! Local tool admission and execution without host persistence.

use super::diagnostic::{
    Cause, Diagnostic, DiagnosticContext, Effects, FailureSite, Operation, PathFact, PathRole,
    Subject, safe_text,
};

/// A failure normalized at an annotation, transport, or persistence boundary.
/// Its cause remains authoritative: no native error is reconstructed from it.
#[derive(Debug)]
pub struct OperationFailure<O> {
    pub diagnostic: Diagnostic,
    pub output: Option<O>,
    provenance: FailureProvenance,
}

/// Local handler authority, deliberately absent from wire and persistence facts.
#[derive(Debug)]
enum FailureProvenance {
    Unspecified,
    SourceFilesystemIo,
}

#[derive(Debug)]
pub enum OperationError<O> {
    Denied(String),
    ArgumentsMustBeObject,
    InvalidBackground,
    BackgroundUnsupported(String),
    InvalidArguments(String),
    InputClosed,
    Cancelled,
    Interrupted,
    Failed(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Failure(Box<OperationFailure<O>>),
}

impl<O> From<std::io::Error> for OperationError<O> {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl<O> From<crate::session::SessionError> for OperationError<O> {
    fn from(error: crate::session::SessionError) -> Self {
        Self::from_diagnostic(Diagnostic::session(&error), None)
    }
}
impl<O> From<std::sync::Arc<crate::session::SessionError>> for OperationError<O> {
    fn from(error: std::sync::Arc<crate::session::SessionError>) -> Self {
        Self::from_diagnostic(Diagnostic::session(&error), None)
    }
}
impl<O> From<serde_json::Error> for OperationError<O> {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
impl<O: std::fmt::Debug> std::error::Error for OperationError<O> {}
impl<O> std::fmt::Display for OperationError<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.diagnostic().render(&Default::default()))
    }
}

impl<O> OperationError<O> {
    pub(crate) fn invalid(error: impl std::fmt::Display) -> Self {
        Self::InvalidArguments(error.to_string())
    }
    pub(crate) fn failed(error: impl std::fmt::Display) -> Self {
        Self::Failed(error.to_string())
    }
    /// Identify a source-filesystem failure at the handler boundary. Presentation
    /// context alone never grants the read policy authority to turn it into data.
    pub(crate) fn source_filesystem_io(error: std::io::Error) -> Self {
        Self::Io(error)
            .annotate(|failure| failure.provenance = FailureProvenance::SourceFilesystemIo)
    }
    pub(crate) fn is_source_filesystem_io(&self) -> bool {
        matches!(self, Self::Failure(failure)
            if matches!(failure.provenance, FailureProvenance::SourceFilesystemIo))
    }

    /// `map_err` adapter: convert a source error and attach the facts its site chose.
    pub fn annotated<E: Into<Self>>(context: DiagnosticContext) -> impl FnOnce(E) -> Self {
        move |error| error.into().context(context)
    }
    #[must_use]
    pub fn with_output(message: impl Into<String>, output: O) -> Self {
        Self::Failed(message.into()).with_result(output)
    }
    #[must_use]
    pub fn with_result(self, output: O) -> Self {
        self.annotate(|failure| failure.output = Some(output))
    }
    #[must_use]
    pub fn context(self, context: DiagnosticContext) -> Self {
        self.annotate(|failure| failure.diagnostic.context = context)
    }
    /// Fill missing boundary facts without replacing a known stage, site or effects.
    #[must_use]
    pub fn fallback_context(self, context: DiagnosticContext) -> Self {
        self.annotate(|failure| failure.diagnostic.context.fallback(context))
    }
    #[must_use]
    pub fn operation(self, operation: Operation, subject: Subject) -> Self {
        self.annotate(|failure| {
            failure.diagnostic.context.operation = operation;
            failure.diagnostic.context.subject = subject;
        })
    }
    #[must_use]
    pub fn at(self, site: FailureSite) -> Self {
        self.annotate(|failure| failure.diagnostic.context.site = site)
    }
    #[must_use]
    pub fn effects(self, effects: Effects) -> Self {
        self.annotate(|failure| failure.diagnostic.context.effects = effects)
    }
    /// Retain classification/context/output while dropping opaque I/O detail.
    #[must_use]
    pub(crate) fn opaque_io(self) -> Self {
        self.annotate(|failure| failure.diagnostic.cause.redact_io_detail())
    }
    fn annotate(self, change: impl FnOnce(&mut OperationFailure<O>)) -> Self {
        let mut failure = self.into_failure();
        change(&mut failure);
        Self::Failure(failure)
    }

    pub fn diagnostic(&self) -> Diagnostic {
        let cause = match self {
            Self::Failure(failure) => return failure.diagnostic.clone(),
            Self::Denied(reason) => Cause::Denied(reason.clone()),
            Self::ArgumentsMustBeObject => {
                Cause::InvalidArguments("tool arguments must be a JSON object".into())
            }
            Self::InvalidBackground => Cause::InvalidArguments("bg must be a boolean".into()),
            Self::BackgroundUnsupported(_) => {
                Cause::InvalidArguments("background execution is unsupported".into())
            }
            Self::InvalidArguments(message) => Cause::InvalidArguments(safe_text(message)),
            Self::InputClosed => Cause::InputClosed,
            Self::Cancelled => Cause::Cancelled,
            Self::Interrupted => Cause::Interrupted,
            Self::Failed(message) => Cause::Message(safe_text(message)),
            Self::Io(error) => Cause::io(error),
            Self::Json(_) => Cause::Json,
        };
        Diagnostic::new(DiagnosticContext::default(), cause)
    }

    pub(crate) fn into_failure(self) -> Box<OperationFailure<O>> {
        match self {
            Self::Failure(failure) => failure,
            error => Box::new(OperationFailure {
                diagnostic: error.diagnostic(),
                output: None,
                provenance: FailureProvenance::Unspecified,
            }),
        }
    }
    /// Extract the one authoritative diagnostic and any completion evidence.
    pub(crate) fn into_parts(self) -> (Diagnostic, Option<O>) {
        let OperationFailure {
            diagnostic, output, ..
        } = *self.into_failure();
        (diagnostic, output)
    }
    pub(crate) fn from_diagnostic(diagnostic: Diagnostic, output: Option<Box<O>>) -> Self {
        Self::Failure(Box::new(OperationFailure {
            diagnostic,
            output: output.map(|output| *output),
            provenance: FailureProvenance::Unspecified,
        }))
    }
    pub(crate) fn try_map_output<P, E>(
        self,
        convert: impl FnOnce(O) -> Result<P, E>,
    ) -> Result<OperationError<P>, E> {
        let OperationFailure {
            diagnostic,
            output,
            provenance,
        } = *self.into_failure();
        Ok(OperationError::Failure(Box::new(OperationFailure {
            diagnostic,
            output: output.map(convert).transpose()?,
            provenance,
        })))
    }
}

#[derive(Debug)]
pub enum AdmissionError {
    Denied(String),
    ArgumentsMustBeObject,
    InvalidBackground,
    BackgroundUnsupported(String),
    InvalidArguments(String),
    Cancelled,
    Failed(String),
    Io(std::io::Error),
    Annotated {
        context: Box<DiagnosticContext>,
        source: Box<Self>,
    },
}
impl std::error::Error for AdmissionError {}
impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.diagnostic().render(&Default::default()))
    }
}
impl From<std::io::Error> for AdmissionError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl AdmissionError {
    pub(crate) fn invalid(error: impl std::fmt::Display) -> Self {
        Self::InvalidArguments(error.to_string())
    }
    /// `map_err` adapter: convert a source error and attach the facts its site chose.
    pub fn annotated<E: Into<Self>>(context: DiagnosticContext) -> impl FnOnce(E) -> Self {
        move |error| error.into().context(context)
    }
    #[must_use]
    pub fn context(self, context: DiagnosticContext) -> Self {
        let context = Box::new(context);
        match self {
            Self::Annotated { source, .. } => Self::Annotated { context, source },
            source => Self::Annotated {
                context,
                source: Box::new(source),
            },
        }
    }
    /// Supply boundary context only when the source did not identify its own stage.
    #[must_use]
    pub fn fallback_context(self, context: DiagnosticContext) -> Self {
        match self {
            Self::Annotated { .. } => self,
            source => source.context(context),
        }
    }
    #[must_use]
    pub fn operation(self, operation: Operation, subject: Subject) -> Self {
        self.annotate(|context| {
            context.operation = operation;
            context.subject = subject;
        })
    }
    #[must_use]
    pub fn at(self, site: FailureSite) -> Self {
        self.annotate(|context| context.site = site)
    }
    #[must_use]
    pub fn effects(self, effects: Effects) -> Self {
        self.annotate(|context| context.effects = effects)
    }
    fn annotate(self, change: impl FnOnce(&mut DiagnosticContext)) -> Self {
        let mut context = self.diagnostic().context;
        change(&mut context);
        self.context(context)
    }
    pub fn unannotated(&self) -> &Self {
        match self {
            Self::Annotated { source, .. } => source.unannotated(),
            source => source,
        }
    }
    pub fn diagnostic(&self) -> Diagnostic {
        let cause = match self {
            Self::Annotated { context, source } => {
                return Diagnostic::new(context.as_ref().clone(), source.diagnostic().cause);
            }
            Self::Denied(reason) => Cause::Denied(reason.clone()),
            Self::ArgumentsMustBeObject => {
                Cause::InvalidArguments("tool arguments must be a JSON object".into())
            }
            Self::InvalidBackground => Cause::InvalidArguments("bg must be a boolean".into()),
            Self::BackgroundUnsupported(_) => {
                Cause::InvalidArguments("background execution is unsupported".into())
            }
            Self::InvalidArguments(message) => Cause::InvalidArguments(safe_text(message)),
            Self::Cancelled => Cause::Cancelled,
            Self::Failed(message) => Cause::Message(safe_text(message)),
            Self::Io(error) => Cause::io(error),
        };
        Diagnostic::new(DiagnosticContext::default(), cause)
    }
}
impl<O> From<AdmissionError> for OperationError<O> {
    fn from(error: AdmissionError) -> Self {
        match error {
            AdmissionError::Denied(value) => Self::Denied(value),
            AdmissionError::ArgumentsMustBeObject => Self::ArgumentsMustBeObject,
            AdmissionError::InvalidBackground => Self::InvalidBackground,
            AdmissionError::BackgroundUnsupported(value) => Self::BackgroundUnsupported(value),
            AdmissionError::InvalidArguments(value) => Self::InvalidArguments(value),
            AdmissionError::Cancelled => Self::Cancelled,
            AdmissionError::Failed(value) => Self::Failed(value),
            AdmissionError::Io(error) => Self::Io(error),
            AdmissionError::Annotated { context, source } => Self::from(*source).context(*context),
        }
    }
}

pub(crate) type LocalError = OperationError<super::output::ProducedOutput>;

use super::output::{CaptureKind, OutputContext, PendingOutput, ProducedOutput, TextCaptureField};
use crate::{
    execution::ExecutionLocation,
    tool::{
        policy::{ApprovalGrant, Capability, CapabilitySet, PermissionUse, ResourceId},
        registry::{Catalog, CatalogBuilder, CatalogEntry, OutputValue},
    },
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{path::Path, sync::Arc, time::Duration};

pub(crate) const CANCELLATION_GRACE: Duration = Duration::from_millis(250);
pub(crate) type LocalCatalogBuilder = CatalogBuilder<LocalContext, ProducedOutput>;
pub(crate) type LocalCatalog = Catalog<LocalContext, ProducedOutput>;

impl OutputValue for ProducedOutput {
    fn from_value(value: Value) -> Self {
        Self::new(value)
    }
    fn with_diagnostic(self, diagnostic: Diagnostic) -> Self {
        self.with_diagnostic(diagnostic)
    }
}

pub(crate) trait LocalAuthorizer: Send + Sync {
    fn authorize(
        &self,
        permissions: Vec<PermissionUse>,
        arguments: Value,
    ) -> BoxFuture<'static, Result<(), AdmissionError>>;
}

#[derive(Clone)]
pub(crate) struct LocalContext {
    location: ExecutionLocation,
    capabilities: CapabilitySet,
    pub(crate) process_environment: crate::remote::backend::ProcessEnvironment,
    cancellation: tokio_util::sync::CancellationToken,
    output: OutputContext,
    authorizer: Arc<dyn LocalAuthorizer>,
    arguments: Arc<Value>,
}

impl LocalContext {
    pub(crate) fn new(
        location: ExecutionLocation,
        capabilities: CapabilitySet,
        process_environment: crate::remote::backend::ProcessEnvironment,
        cancellation: tokio_util::sync::CancellationToken,
        output: OutputContext,
        authorizer: Arc<dyn LocalAuthorizer>,
        arguments: Value,
    ) -> Self {
        Self {
            location,
            capabilities,
            process_environment,
            cancellation,
            output,
            authorizer,
            arguments: Arc::new(arguments),
        }
    }

    pub(crate) fn with_arguments(mut self, arguments: Value) -> Self {
        self.arguments = Arc::new(arguments);
        self
    }

    pub(crate) fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }
    pub(crate) fn execution_location(&self) -> &ExecutionLocation {
        &self.location
    }
    pub(crate) fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancellation.clone()
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
    pub(crate) async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
    pub(crate) async fn pending_stream_capture(
        &self,
        field: &str,
        kind: CaptureKind,
    ) -> Result<PendingOutput, LocalError> {
        Ok(self.output.pending_stream_capture(field, kind).await?)
    }
    pub(crate) async fn text_capture(
        &self,
        field: TextCaptureField,
    ) -> Result<PendingOutput, LocalError> {
        Ok(self.output.text_capture(field).await?)
    }
    pub(crate) async fn store_image(
        &self,
        path: Option<String>,
        image: &crate::media::Image,
    ) -> Result<crate::media::ImageRef, LocalError> {
        Ok(self.output.store_image(path, image).await?)
    }
    pub(crate) async fn authorize_network(
        &self,
        normalized_origin: &str,
    ) -> Result<(), LocalError> {
        let mut arguments = (*self.arguments).clone();
        if let Some(object) = arguments.as_object_mut() {
            object.insert(
                "network_origin".to_owned(),
                Value::String(normalized_origin.to_owned()),
            );
        }
        self.authorizer
            .authorize(
                vec![PermissionUse::new(
                    Capability::Network,
                    ResourceId::network(&self.location.target, normalized_origin),
                )],
                arguments,
            )
            .await
            .map_err(Into::into)
    }
}

impl LocalCatalog {
    pub(crate) fn builtins() -> Result<Self, crate::tool::RegistryError> {
        let mut builder = LocalCatalogBuilder::default();
        crate::tool::builtins::register_local_tools(&mut builder)?;
        crate::tool::builtins::skill_transfer::register_worker(&mut builder)?;
        Ok(builder.build())
    }

    pub(crate) async fn run(
        &self,
        name: &str,
        mut arguments: Value,
        context: LocalContext,
        authorization_root: &Path,
    ) -> Result<ProducedOutput, LocalError> {
        let tool = self
            .get(name)
            .ok_or_else(|| LocalError::InvalidArguments(format!("unknown tool `{name}`")))?;
        let surface = self.surface(context.capabilities());
        let spec = surface
            .get(name)
            .ok_or_else(|| LocalError::Denied(format!("tool `{name}` is unavailable")))?;
        spec.validate_arguments(&arguments)?;
        let original_arguments = arguments.clone();
        tool.validate_arguments(&arguments)?;
        let path = preflight_path_arguments(
            &tool,
            &context.location.target,
            &context.location.workspace,
            authorization_root,
            &mut arguments,
        )
        .await?;
        let argument_permissions = tool.argument_permissions(&context.location, &arguments)?;
        let mut capabilities = tool.capabilities();
        for permission in &argument_permissions {
            if !context.capabilities.contains(permission.capability) {
                return Err(LocalError::Denied(
                    "required capability is unavailable".to_owned(),
                ));
            }
            capabilities.retain(|candidate| *candidate != permission.capability);
        }
        if matches!(path.outcome, PathOutcome::Ready) {
            for permission in &path.permissions {
                capabilities.retain(|candidate| *candidate != permission.capability);
            }
        }
        let mut permissions =
            scope_capabilities(capabilities, &context.location, tool.permission_resource());
        permissions.extend(path.permissions);
        permissions.extend(argument_permissions);
        if permissions
            .iter()
            .any(|permission| !context.capabilities.contains(permission.capability))
        {
            return Err(LocalError::Denied(
                "required capability is unavailable".to_owned(),
            ));
        }
        context
            .authorizer
            .authorize(permissions, original_arguments.clone())
            .await?;
        if context.is_cancelled() {
            return Err(LocalError::Cancelled);
        }
        match path.outcome {
            PathOutcome::Ready => {
                let fallback = DiagnosticContext {
                    paths: path.paths,
                    ..Default::default()
                };
                tool.admit(arguments, &original_arguments)?
                    .call(context.with_arguments(original_arguments))
                    .await
                    .map(|mut output| {
                        if let Some(diagnostic) = &mut output.diagnostic {
                            diagnostic.context.fallback(fallback.clone());
                        }
                        output
                    })
                    .map_err(|error| error.fallback_context(fallback))
            }
            PathOutcome::ReadError { value, diagnostic } => {
                Ok(ProducedOutput::new(value).with_diagnostic(*diagnostic))
            }
        }
    }
}

pub(crate) struct PathPreflight {
    pub(crate) permissions: Vec<PermissionUse>,
    pub(crate) outcome: PathOutcome,
    pub(crate) paths: Vec<PathFact>,
}

pub(crate) enum PathOutcome {
    Ready,
    ReadError {
        value: Value,
        diagnostic: Box<Diagnostic>,
    },
}

use crate::tool::builtins::workspace::{lexical_path, resolve_for_authorization};

pub(crate) async fn preflight_path_arguments<C: Send + 'static, O: OutputValue>(
    tool: &CatalogEntry<C, O>,
    target: &str,
    workspace: &std::path::Path,
    authorization_root: &std::path::Path,
    arguments: &mut Value,
) -> Result<PathPreflight, AdmissionError> {
    if !arguments.is_object() {
        return Err(AdmissionError::ArgumentsMustBeObject);
    }
    let mut permissions = Vec::new();
    let mut paths = Vec::new();
    let mut outcome = PathOutcome::Ready;
    for spec in tool.path_arguments(arguments)? {
        let Some(input) = spec.input(arguments)?.map(str::to_owned) else {
            continue;
        };
        let resolved = match resolve_for_authorization(workspace, &input, spec.kind).await {
            Ok(resolved) => resolved,
            Err(error) => {
                let mut context = error.diagnostic().context.path(PathRole::Requested, &input);
                if spec.name() == "cwd" {
                    context.subject = Subject::working_directory(lexical_path(workspace, &input)?);
                    context.effects = Effects::NotStarted;
                }
                context.site =
                    FailureSite::Execution(ExecutionLocation::named(target, workspace.to_owned()));
                let error = error.context(context);
                let diagnostic = error.diagnostic();
                let Some(output) = tool.read_error_output(&input, &diagnostic) else {
                    return Err(error);
                };
                // Canonicalization failed, so this path is not proven to be within
                // the authorization root. Require exact path authorization even for
                // apparently local paths, then return the captured failure without
                // retrying the handler (which could now access a changed target).
                let path = lexical_path(workspace, &input)?;
                let capability = spec.access.capability();
                path_text(&path)?;
                let resource = ResourceId::path(target, &path);
                permissions.push(
                    PermissionUse::new(capability, resource.clone())
                        .with_grant(ApprovalGrant::exact(capability, resource)),
                );
                outcome = PathOutcome::ReadError {
                    value: output,
                    diagnostic: Box::new(diagnostic),
                };
                continue;
            }
        };
        // Both the existing handler JSON and permission resource are Unicode
        // boundaries. Never authorize a replacement-character alias.
        let value = Value::String(
            path_text(&resolved.path)
                .map_err(|error| {
                    error.context(
                        DiagnosticContext::new(Operation::Validate, Subject::path(&resolved.path))
                            .path(PathRole::Requested, &input)
                            .path(PathRole::Resolved, &resolved.path),
                    )
                })?
                .to_owned(),
        );
        paths.push(PathFact {
            role: PathRole::Requested,
            path: input.into(),
        });
        paths.push(PathFact {
            role: PathRole::Resolved,
            path: resolved.path.clone(),
        });
        spec.rewrite(arguments, value)?;
        if matches!(spec.binding, crate::tool::registry::PathBinding::Pointer(_))
            || !resolved.path.starts_with(authorization_root)
        {
            permissions.push(resolved.permission(spec.access.capability(), target));
        }
    }
    Ok(PathPreflight {
        permissions,
        outcome,
        paths,
    })
}

/// Validate the existing string-only permission/wire boundary without changing
/// native path identity. Lossless byte-path protocols are a separate migration.
pub(crate) fn path_text(path: &std::path::Path) -> Result<&str, AdmissionError> {
    path.to_str().ok_or_else(|| {
        AdmissionError::InvalidArguments(
            "native path cannot be represented losslessly by the permission or wire format"
                .to_owned(),
        )
    })
}

pub(crate) fn scope_capabilities(
    capabilities: Vec<Capability>,
    location: &ExecutionLocation,
    override_resource: Option<&ResourceId>,
) -> Vec<PermissionUse> {
    let resource = override_resource
        .cloned()
        .unwrap_or_else(|| ResourceId::workspace(&location.target, &location.workspace));
    capabilities
        .into_iter()
        .map(|capability| PermissionUse::new(capability, resource.clone()))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::output::{CaptureEvent, CaptureId, OutputEvent, OutputSink};
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct CapturedOutput(Mutex<BTreeMap<CaptureId, Vec<u8>>>);

    impl CapturedOutput {
        pub(crate) fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().values().flatten().copied().collect()
        }
    }

    impl OutputSink for CapturedOutput {
        fn send(&self, event: OutputEvent) -> std::io::Result<()> {
            let OutputEvent::Capture(event) = event else {
                return Ok(());
            };
            let mut captures = self.0.lock().unwrap();
            match event {
                CaptureEvent::Open { id, .. } => {
                    captures.insert(id, Vec::new());
                }
                CaptureEvent::Write { id, data } => captures.get_mut(&id).unwrap().extend(data),
                CaptureEvent::Truncate { id, length } => captures
                    .get_mut(&id)
                    .unwrap()
                    .truncate(usize::try_from(length).unwrap()),
                CaptureEvent::Discard { id } => {
                    captures.remove(&id);
                }
                CaptureEvent::Finish { .. } => {}
            }
            Ok(())
        }
    }

    #[derive(Default)]
    pub(crate) struct Authorizations(Mutex<Vec<Vec<PermissionUse>>>);

    impl LocalAuthorizer for Authorizations {
        fn authorize(
            &self,
            permissions: Vec<PermissionUse>,
            _: Value,
        ) -> BoxFuture<'static, Result<(), AdmissionError>> {
            self.0.lock().unwrap().push(permissions);
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn local_admission_preserves_read_errors_and_capability_checks_without_persistence() {
        let root = tempfile::tempdir().unwrap();
        let catalog = LocalCatalog::builtins().unwrap();
        let authorizations = Arc::new(Authorizations::default());
        let output = OutputContext::new(Arc::new(CapturedOutput::default()));
        let context = LocalContext::new(
            ExecutionLocation::root(root.path().to_owned()),
            [Capability::Read].into_iter().collect(),
            Default::default(),
            tokio_util::sync::CancellationToken::new(),
            output.clone(),
            authorizations.clone(),
            serde_json::json!({"path":"missing"}),
        );
        let missing = catalog
            .run(
                "read",
                serde_json::json!({"path":"missing"}),
                context.clone(),
                root.path(),
            )
            .await
            .unwrap();
        assert_eq!(missing.value["error"]["code"], "not_found");
        {
            let permissions = authorizations.0.lock().unwrap();
            assert_eq!(permissions.len(), 1);
            assert!(
                permissions[0]
                    .iter()
                    .any(|permission| matches!(permission.resource, ResourceId::Path { .. }))
            );
        }
        let denied = catalog
            .run(
                "write",
                serde_json::json!({"path":"missing","content":"denied"}),
                context,
                root.path(),
            )
            .await;
        assert!(matches!(denied, Err(LocalError::Denied(_))));
        assert!(!root.path().join("missing").exists());
        assert_eq!(authorizations.0.lock().unwrap().len(), 1);
        output.settle().await.unwrap();
    }
}
