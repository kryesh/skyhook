//! Local tool admission and execution without host persistence.

use std::io;

use super::diagnostic::{
    Cause, Diagnostic, Effects, FailureSite, Operation, PartialContext, PartialDiagnostic,
    PathFact, PathRole, Subject, safe_text,
};

/// A failure normalized at an annotation, transport, or persistence boundary.
/// Its cause remains authoritative: no native error is reconstructed from it.
/// Boxed so every handler `Result` stays pointer-sized.
#[derive(Debug)]
pub struct OperationError<O>(Box<Facts<O>>);

#[derive(Debug)]
struct Facts<O> {
    diagnostic: PartialDiagnostic,
    output: Option<O>,
    local: LocalFacts,
}

/// Local handler evidence and authority, deliberately absent from wire and
/// persistence facts.
#[derive(Debug, Default)]
struct LocalFacts {
    provenance: FailureProvenance,
    native_io: Option<io::Error>,
}

#[derive(Debug, Default)]
enum FailureProvenance {
    #[default]
    Unspecified,
    SourceFilesystemIo,
}

/// Admission never produces partial output.
pub type AdmissionError = OperationError<std::convert::Infallible>;

impl<O> From<io::Error> for OperationError<O> {
    fn from(error: io::Error) -> Self {
        Self::io(error)
    }
}
impl<O> From<crate::session::SessionError> for OperationError<O> {
    fn from(error: crate::session::SessionError) -> Self {
        Self::from_facts(PartialDiagnostic::session(&error), None)
    }
}
impl<O> From<std::sync::Arc<crate::session::SessionError>> for OperationError<O> {
    fn from(error: std::sync::Arc<crate::session::SessionError>) -> Self {
        Self::from_facts(PartialDiagnostic::session(&error), None)
    }
}
impl<O> From<serde_json::Error> for OperationError<O> {
    fn from(_: serde_json::Error) -> Self {
        Self::cause(Cause::Json)
    }
}
impl<O: OutputValue> From<AdmissionError> for OperationError<O> {
    fn from(error: AdmissionError) -> Self {
        let Facts {
            diagnostic,
            output,
            local,
        } = *error.0;
        Self(Box::new(Facts {
            diagnostic,
            output: output.map(|never| match never {}),
            local,
        }))
    }
}
impl<O: std::fmt::Debug> std::error::Error for OperationError<O> {}
impl<O> std::fmt::Display for OperationError<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.diagnostic().render(&Default::default()))
    }
}

impl<O> OperationError<O> {
    pub(crate) fn cause(cause: Cause) -> Self {
        Self::from_facts(
            PartialDiagnostic::new(PartialContext::default(), cause),
            None,
        )
    }
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::cause(Cause::Denied(reason.into()))
    }
    pub fn invalid_arguments(message: impl std::fmt::Display) -> Self {
        Self::cause(Cause::InvalidArguments(safe_text(&message.to_string())))
    }
    pub(crate) fn arguments_must_be_object() -> Self {
        Self::invalid_arguments("tool arguments must be a JSON object")
    }
    pub fn failed(error: impl std::fmt::Display) -> Self {
        Self::cause(Cause::Message(safe_text(&error.to_string())))
    }
    pub fn cancelled() -> Self {
        Self::cause(Cause::Cancelled)
    }
    pub fn interrupted() -> Self {
        Self::cause(Cause::Interrupted)
    }
    pub fn input_closed() -> Self {
        Self::cause(Cause::InputClosed)
    }
    /// Classify a native I/O error, retaining it locally as evidence.
    pub fn io(error: io::Error) -> Self {
        let mut this = Self::cause(Cause::io(&error));
        this.0.local.native_io = Some(error);
        this
    }
    /// The native error this failure was classified from, when it arose locally.
    pub(crate) fn native_io(&self) -> Option<&io::Error> {
        self.0.local.native_io.as_ref()
    }
    /// Identify a source-filesystem failure at the handler boundary. Presentation
    /// context alone never grants the read policy authority to turn it into data.
    pub(crate) fn source_filesystem_io(error: io::Error) -> Self {
        let mut this = Self::io(error);
        this.0.local.provenance = FailureProvenance::SourceFilesystemIo;
        this
    }
    pub(crate) fn is_source_filesystem_io(&self) -> bool {
        matches!(
            self.0.local.provenance,
            FailureProvenance::SourceFilesystemIo
        )
    }

    /// `map_err` adapter: convert a source error and attach the facts its site chose.
    pub fn annotated<E: Into<Self>>(context: PartialContext) -> impl FnOnce(E) -> Self {
        move |error| error.into().context(context)
    }
    #[must_use]
    pub fn with_output(message: impl std::fmt::Display, output: O) -> Self {
        Self::failed(message).with_result(output)
    }
    #[must_use]
    pub fn with_result(mut self, output: O) -> Self {
        self.0.output = Some(output);
        self
    }
    /// Replace every fact with those the failure site chose.
    #[must_use]
    pub fn context(mut self, context: PartialContext) -> Self {
        self.0.diagnostic.context = context;
        self
    }
    /// Fill the facts still unset without replacing a known stage, site or effects.
    #[must_use]
    pub fn or(mut self, context: PartialContext) -> Self {
        self.0.diagnostic = self.0.diagnostic.or(context);
        self
    }
    #[must_use]
    pub fn operation(mut self, operation: Operation, subject: Subject) -> Self {
        let context = std::mem::take(&mut self.0.diagnostic.context);
        self.0.diagnostic.context = PartialContext::new(operation, subject).or(context);
        self
    }
    #[must_use]
    pub fn at(mut self, site: FailureSite) -> Self {
        self.0.diagnostic.context = self.0.diagnostic.context.at(site);
        self
    }
    #[must_use]
    pub fn effects(mut self, effects: Effects) -> Self {
        self.0.diagnostic.context = self.0.diagnostic.context.effects(effects);
        self
    }
    /// Retain classification/context/output while dropping opaque I/O detail.
    #[must_use]
    pub(crate) fn opaque_io(mut self) -> Self {
        self.0.diagnostic.cause.redact_io_detail();
        self
    }

    /// The facts resolved for presentation, persistence or the wire.
    pub fn diagnostic(&self) -> Diagnostic {
        self.0.diagnostic.clone().resolve()
    }
    /// The facts as the failure site left them.
    pub(crate) fn facts(&self) -> &PartialDiagnostic {
        &self.0.diagnostic
    }
    /// Extract the one authoritative diagnostic and any completion evidence.
    pub(crate) fn into_parts(self) -> (Diagnostic, Option<O>) {
        let (diagnostic, output) = self.into_facts();
        (diagnostic.resolve(), output)
    }
    pub(crate) fn into_facts(self) -> (PartialDiagnostic, Option<O>) {
        let Facts {
            diagnostic, output, ..
        } = *self.0;
        (diagnostic, output)
    }
    /// Restore a resolved diagnostic; every fact it carries counts as chosen.
    pub(crate) fn from_diagnostic(diagnostic: Diagnostic, output: Option<O>) -> Self {
        Self::from_facts(diagnostic.into(), output)
    }
    pub(crate) fn from_facts(diagnostic: PartialDiagnostic, output: Option<O>) -> Self {
        Self(Box::new(Facts {
            diagnostic,
            output,
            local: LocalFacts::default(),
        }))
    }
    pub(crate) fn try_map_output<P, E>(
        self,
        convert: impl FnOnce(O) -> Result<P, E>,
    ) -> Result<OperationError<P>, E> {
        let Facts {
            diagnostic,
            output,
            local,
        } = *self.0;
        Ok(OperationError(Box::new(Facts {
            diagnostic,
            output: output.map(convert).transpose()?,
            local,
        })))
    }
}

pub(crate) type LocalError = OperationError<super::output::ProducedOutput>;

use super::output::{
    CaptureKind, FieldPointer, OutputContext, PendingOutput, ProducedOutput, TextCaptureField,
};
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
    fn with_diagnostic(self, diagnostic: PartialDiagnostic) -> Self {
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
        field: FieldPointer,
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

    /// The wire carries resolved facts, so the worker names the tool it ran
    /// before sending; the host still rebinds the site to its connection.
    pub(crate) async fn run(
        &self,
        name: &str,
        arguments: Value,
        context: LocalContext,
        authorization_root: &Path,
    ) -> Result<ProducedOutput, LocalError> {
        let fallback = PartialContext::new(Operation::Execute, Subject::Tool(name.to_owned()));
        self.run_admitted(name, arguments, context, authorization_root)
            .await
            .map(|mut output| {
                output.diagnostic = output.diagnostic.map(|d| d.or(fallback.clone()));
                output
            })
            .map_err(|error| error.or(fallback))
    }

    async fn run_admitted(
        &self,
        name: &str,
        mut arguments: Value,
        context: LocalContext,
        authorization_root: &Path,
    ) -> Result<ProducedOutput, LocalError> {
        let tool = self
            .get(name)
            .ok_or_else(|| LocalError::invalid_arguments(format!("unknown tool `{name}`")))?;
        let surface = self.surface(context.capabilities());
        let spec = surface
            .get(name)
            .ok_or_else(|| LocalError::denied(format!("tool `{name}` is unavailable")))?;
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
                return Err(LocalError::denied("required capability is unavailable"));
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
            return Err(LocalError::denied("required capability is unavailable"));
        }
        context
            .authorizer
            .authorize(permissions, original_arguments.clone())
            .await?;
        if context.is_cancelled() {
            return Err(LocalError::cancelled());
        }
        match path.outcome {
            PathOutcome::Ready => {
                let fallback = PartialContext::default().paths(path.paths);
                tool.admit(arguments, &original_arguments)?
                    .call(context.with_arguments(original_arguments))
                    .await
                    .map(|mut output| {
                        output.diagnostic = output.diagnostic.map(|d| d.or(fallback.clone()));
                        output
                    })
                    .map_err(|error| error.or(fallback))
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
        diagnostic: Box<PartialDiagnostic>,
    },
}

use crate::tool::builtins::workspace::{lexical_path, resolve_for_authorization};

pub(crate) async fn preflight_path_arguments<C: Send + 'static, O: OutputValue>(
    tool: &CatalogEntry<C, O>,
    target: &crate::target::TargetRef,
    workspace: &std::path::Path,
    authorization_root: &std::path::Path,
    arguments: &mut Value,
) -> Result<PathPreflight, AdmissionError> {
    if !arguments.is_object() {
        return Err(AdmissionError::arguments_must_be_object());
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
                // The site is known here; workspace resolution chose the rest.
                let site = FailureSite::Execution(ExecutionLocation {
                    target: target.clone(),
                    workspace: workspace.to_owned(),
                });
                // Preflight adds what it knows to the resolver's facts; nothing is
                // resolved here, so the tool's own fallback still fills the rest.
                let mut facts = error
                    .facts()
                    .context
                    .clone()
                    .at(site)
                    .path(PathRole::Requested, &input);
                if spec.name() == "cwd" {
                    facts = facts
                        .subject(Subject::working_directory(lexical_path(workspace, &input)?))
                        .effects(Effects::NotStarted);
                }
                let error = error.context(facts);
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
                    diagnostic: Box::new(error.into_facts().0),
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
                        PartialContext::new(Operation::Validate, Subject::path(&resolved.path))
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
        AdmissionError::invalid_arguments(
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
        // Preflight adds only what it knows: the site and the requested path. The
        // resolver's own facts stand, and nothing claims effects it cannot see.
        let facts = missing.diagnostic.clone().unwrap().resolve().context;
        assert_eq!(facts.operation, Operation::Canonicalize);
        assert_eq!(facts.subject, Subject::path(root.path().join("missing")));
        assert_eq!(
            facts.site,
            FailureSite::Execution(ExecutionLocation::root(root.path().to_owned()))
        );
        assert_eq!(facts.effects, Effects::Unknown);
        assert!(facts.paths.iter().any(|fact| {
            fact.role == PathRole::Requested && fact.path == std::path::Path::new("missing")
        }));
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
        assert!(denied.is_err_and(|error| error.diagnostic().is_denial()));
        assert!(!root.path().join("missing").exists());
        assert_eq!(authorizations.0.lock().unwrap().len(), 1);
        output.settle().await.unwrap();
    }
}
