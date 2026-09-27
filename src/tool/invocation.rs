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
impl<O> From<crate::fs::RegularFileError> for OperationError<O> {
    fn from(error: crate::fs::RegularFileError) -> Self {
        match error {
            crate::fs::RegularFileError::Io(error) => Self::io(error),
            error => Self::failed(error),
        }
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
    pub(crate) fn unavailable(tool: &str) -> Self {
        Self::cause(Cause::Unavailable {
            tool: tool.to_owned(),
        })
    }
    pub(crate) fn unknown_tool(tool: &str) -> Self {
        Self::cause(Cause::UnknownTool {
            tool: tool.to_owned(),
        })
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
    or_fallback,
};
use crate::{
    execution::ExecutionLocation,
    tool::{
        authorization::Reauthorization,
        policy::{Capability, CapabilitySet, PathText, PermissionUse, ResourceId},
        registry::{
            AgentLevel, Catalog, CatalogBuilder, CatalogEntry, OutputValue, PathArgument, PathKind,
            PathScope,
        },
    },
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{path::Path, sync::Arc};

pub(crate) type LocalCatalogBuilder = CatalogBuilder<LocalContext, ProducedOutput>;
pub(crate) type LocalCatalog = Catalog<LocalContext, ProducedOutput>;

pub(crate) trait LocalAuthorizer: Send + Sync {
    /// Authorize under the invocation's own authorization arguments.
    fn authorize(&self, request: Reauthorization)
    -> BoxFuture<'static, Result<(), AdmissionError>>;
}

#[derive(Clone)]
pub(crate) struct LocalContext {
    location: ExecutionLocation,
    capabilities: CapabilitySet,
    pub(crate) process_environment: crate::remote::backend::ProcessEnvironment,
    cancellation: tokio_util::sync::CancellationToken,
    output: OutputContext,
    authorizer: Arc<dyn LocalAuthorizer>,
    source: Option<crate::tool::source::Source>,
}

impl LocalContext {
    pub(crate) fn new(
        location: ExecutionLocation,
        capabilities: CapabilitySet,
        process_environment: crate::remote::backend::ProcessEnvironment,
        cancellation: tokio_util::sync::CancellationToken,
        output: OutputContext,
        authorizer: Arc<dyn LocalAuthorizer>,
    ) -> Self {
        Self {
            location,
            capabilities,
            process_environment,
            cancellation,
            output,
            authorizer,
            source: None,
        }
    }

    /// Attach the opened source argument.
    pub(crate) fn with_source(mut self, source: Option<crate::tool::source::Source>) -> Self {
        self.source = source;
        self
    }

    /// The source argument; the executor opens it for every call naming one.
    pub(crate) fn source(&self) -> Result<&crate::tool::source::Source, LocalError> {
        self.source
            .as_ref()
            .ok_or_else(|| LocalError::failed("source was not opened"))
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

    /// Authorize permissions this machine derived while running the call.
    pub(crate) async fn authorize(
        &self,
        permissions: Vec<PermissionUse>,
    ) -> Result<(), LocalError> {
        let request = Reauthorization::Permissions(permissions);
        Ok(self.authorizer.authorize(request).await?)
    }

    pub(crate) async fn authorize_network(&self, origin: &str) -> Result<(), LocalError> {
        let request = Reauthorization::Network(origin.to_owned());
        Ok(self.authorizer.authorize(request).await?)
    }
}

impl LocalCatalog {
    pub(crate) fn builtins() -> Result<Self, crate::tool::RegistryError> {
        let mut builder = LocalCatalogBuilder::default();
        crate::tool::builtins::register_local_tools(&mut builder)?;
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
        let result = self.run_admitted(name, arguments, context, authorization_root);
        or_fallback(result.await, fallback)
    }

    async fn run_admitted(
        &self,
        name: &str,
        arguments: Value,
        context: LocalContext,
        authorization_root: &Path,
    ) -> Result<ProducedOutput, LocalError> {
        let tool = self
            .get(name)
            .ok_or_else(|| LocalError::unknown_tool(name))?;
        let spec = (tool.spec(context.capabilities(), AgentLevel::Root))
            .ok_or_else(|| LocalError::unavailable(name))?;
        let arguments = spec.validate_arguments(arguments)?;
        let mut admitted = tool.admit(&arguments)?;
        let derived = admitted.permissions(&context.location)?;
        let path = preflight_path_arguments(
            &tool,
            admitted.paths(),
            &context.location,
            authorization_root,
        )
        .await?;
        let permissions = assemble_permissions(
            &tool,
            &context.location,
            &context.capabilities,
            derived,
            &path,
            false,
        )?;
        (context.authorizer)
            .authorize(Reauthorization::Permissions(permissions))
            .await?;
        if context.is_cancelled() {
            return Err(LocalError::cancelled());
        }
        match path.outcome {
            PathOutcome::Ready => {
                let fallback = PartialContext::default().paths(path.paths);
                let result = tool.invoke(admitted).call(context);
                or_fallback(result.await, fallback)
            }
            PathOutcome::ReadError { value, diagnostic } => {
                Ok(ProducedOutput::new(value).with_diagnostic(*diagnostic))
            }
        }
    }
}

/// The permissions an invocation needs, wherever it is planned. Permissions its
/// arguments derive replace their capabilities' static scope, as do its path
/// permissions once every path resolved (an unresolved read still needs the
/// workspace scope); the capabilities left are scoped to the tool's resource or
/// workspace. A `forwarded` invocation's destination authorizes its own path
/// and network permissions and forwards them, so they are not asked for twice.
/// A capability this context lacks makes the tool unavailable, not denied.
pub(crate) fn assemble_permissions<C: Send + 'static, O: OutputValue>(
    tool: &CatalogEntry<C, O>,
    location: &ExecutionLocation,
    available: &CapabilitySet,
    arguments: Vec<PermissionUse>,
    path: &PathPreflight,
    forwarded: bool,
) -> Result<Vec<PermissionUse>, AdmissionError> {
    let mut covered = arguments.iter().collect::<Vec<_>>();
    if matches!(path.outcome, PathOutcome::Ready) {
        covered.extend(&path.permissions);
    }
    let mut capabilities = tool.capabilities();
    capabilities.retain(|capability| {
        !covered
            .iter()
            .any(|permission| permission.capability == *capability)
    });
    let mut permissions = scope_capabilities(capabilities, location, tool.permission_resource())?;
    permissions.extend(path.permissions.iter().cloned());
    permissions.extend(arguments);
    if (permissions.iter()).any(|permission| !available.contains(permission.capability)) {
        return Err(AdmissionError::unavailable(tool.name()));
    }
    if forwarded {
        permissions.retain(|permission| !permission.resource.resolved_at_destination());
    }
    Ok(permissions)
}

pub(crate) struct PathPreflight {
    pub(crate) permissions: Vec<PermissionUse>,
    pub(crate) outcome: PathOutcome,
    pub(crate) paths: Vec<PathFact>,
}

impl PathPreflight {
    /// Nothing resolved here: the call's paths are resolved where it runs.
    pub(crate) const fn deferred() -> Self {
        Self {
            permissions: Vec::new(),
            outcome: PathOutcome::Ready,
            paths: Vec::new(),
        }
    }
}

pub(crate) enum PathOutcome {
    Ready,
    ReadError {
        value: Value,
        diagnostic: Box<PartialDiagnostic>,
    },
}

use crate::tool::path::{lexical_path, resolve_for_authorization};

/// Resolve each path argument where the invocation runs and replace it with its
/// resolved spelling.
pub(crate) async fn preflight_path_arguments<C: Send + 'static, O: OutputValue>(
    tool: &CatalogEntry<C, O>,
    arguments: Vec<PathArgument<'_>>,
    location: &ExecutionLocation,
    authorization_root: &Path,
) -> Result<PathPreflight, AdmissionError> {
    let ExecutionLocation { target, workspace } = location;
    let mut preflight = PathPreflight::deferred();
    for argument in arguments {
        let input = argument.path.clone();
        let capability = argument.access.capability();
        let resolved = match resolve_for_authorization(workspace, &input, argument.kind).await {
            Ok(resolved) => resolved,
            Err(error) => {
                // Preflight adds what it knows to the resolver's facts; nothing is
                // resolved here, so the tool's own fallback still fills the rest.
                let mut facts = error
                    .facts()
                    .context
                    .clone()
                    .at(FailureSite::Execution(location.clone()))
                    .path(PathRole::Requested, &input);
                if argument.kind == PathKind::WorkingDirectory {
                    facts = facts
                        .subject(Subject::working_directory(lexical_path(workspace, &input)?))
                        .effects(Effects::NotStarted);
                }
                let error = error.context(facts);
                let diagnostic = error.diagnostic();
                let Some(output) = tool.read_error_output(&diagnostic) else {
                    return Err(error);
                };
                // Canonicalization failed, so this path is not proven to be within
                // the authorization root. Require exact path authorization even for
                // apparently local paths, then return the captured failure without
                // retrying the handler (which could now access a changed target).
                let path = PathText::new(lexical_path(workspace, &input)?)?;
                preflight.permissions.push(PermissionUse::exact(
                    capability,
                    ResourceId::path(target, &path),
                ));
                preflight.outcome = PathOutcome::ReadError {
                    value: output,
                    diagnostic: Box::new(error.into_facts().0),
                };
                continue;
            }
        };
        resolved.path.as_str().clone_into(argument.path);
        preflight.paths.push(PathFact {
            role: PathRole::Requested,
            path: input.into(),
        });
        preflight.paths.push(PathFact {
            role: PathRole::Resolved,
            path: resolved.path.as_path().to_owned(),
        });
        preflight.permissions.extend(match argument.scope {
            PathScope::Exact => Some(resolved.permission(capability, target)),
            PathScope::Workspace => {
                resolved.permission_outside(authorization_root, capability, target)
            }
        });
    }
    Ok(preflight)
}

/// Scope capabilities to `resource`, or else the workspace, which is spelled
/// only when a capability needs it.
pub(crate) fn scope_capabilities(
    capabilities: Vec<Capability>,
    location: &ExecutionLocation,
    resource: Option<&ResourceId>,
) -> Result<Vec<PermissionUse>, AdmissionError> {
    if capabilities.is_empty() {
        return Ok(Vec::new());
    }
    let resource = match resource {
        Some(resource) => resource.clone(),
        None => ResourceId::workspace(&location.target, &PathText::new(&location.workspace)?),
    };
    Ok(capabilities
        .into_iter()
        .map(|capability| PermissionUse::new(capability, resource.clone()))
        .collect())
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
    pub(crate) struct Authorizations(Mutex<Vec<Reauthorization>>);

    impl LocalAuthorizer for Authorizations {
        fn authorize(
            &self,
            request: Reauthorization,
        ) -> BoxFuture<'static, Result<(), AdmissionError>> {
            self.0.lock().unwrap().push(request);
            Box::pin(async { Ok(()) })
        }
    }

    /// Only a declared working directory fails as one; a path argument that is
    /// merely named `cwd` keeps its resolver's facts.
    #[tokio::test]
    async fn only_a_declared_working_directory_fails_as_one() {
        use crate::tool::{ToolOptions, policy::PathAccess};
        let root = tempfile::tempdir().unwrap();
        let mut builder = LocalCatalogBuilder::default();
        crate::tool::builtins::register_local_tools(&mut builder).unwrap();
        let options = ToolOptions::default().argument_paths(|arguments: &mut Value| {
            let Some(Value::String(cwd)) = arguments.get_mut("cwd") else {
                return Vec::new();
            };
            vec![PathArgument::new(cwd, PathAccess::Read, PathKind::Existing)]
        });
        builder
            .register_dynamic(
                "named_cwd",
                "",
                serde_json::json!({"type":"object"}),
                options,
                |_, _| async { Ok(ProducedOutput::new(Value::Null)) },
            )
            .unwrap();
        let catalog = builder.build();
        let context = LocalContext::new(
            ExecutionLocation::root(root.path().to_owned()),
            CapabilitySet::default(),
            Default::default(),
            tokio_util::sync::CancellationToken::new(),
            OutputContext::new(Arc::new(CapturedOutput::default())),
            Arc::new(Authorizations::default()),
        );
        let missing = root.path().join("missing");
        for (tool, subject, effects) in [
            (
                "exec",
                Subject::working_directory(&missing),
                Effects::NotStarted,
            ),
            ("named_cwd", Subject::path(&missing), Effects::Unknown),
        ] {
            let arguments = serde_json::json!({"command":["true"], "cwd":"missing"});
            let error = (catalog.run(tool, arguments, context.clone(), root.path()))
                .await
                .unwrap_err();
            let facts = error.diagnostic().context;
            assert_eq!((facts.subject, facts.effects), (subject, effects), "{tool}");
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
            let requests = authorizations.0.lock().unwrap();
            let [Reauthorization::Permissions(permissions)] = &requests[..] else {
                panic!("one admission: {requests:?}");
            };
            assert!(
                permissions
                    .iter()
                    .any(|permission| matches!(permission.resource, ResourceId::Path { .. }))
            );
        }
        // A missing capability makes the tool unavailable; it is not a denial.
        let unavailable = catalog
            .run(
                "write",
                serde_json::json!({"path":"missing","content":"denied"}),
                context,
                root.path(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            unavailable.diagnostic().cause,
            Cause::Unavailable {
                tool: "write".into()
            }
        );
        assert!(!root.path().join("missing").exists());
        assert_eq!(authorizations.0.lock().unwrap().len(), 1);
        output.settle().await.unwrap();
    }
}
