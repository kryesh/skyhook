//! Local tool admission and execution without host persistence.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OperationError<O> {
    #[error("operation denied: {0}")]
    Denied(String),
    #[error("tool arguments must be a JSON object")]
    ArgumentsMustBeObject,
    #[error("`bg` must be a boolean")]
    InvalidBackground,
    #[error("tool `{0}` does not support background execution")]
    BackgroundUnsupported(String),
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("tool input channel is closed")]
    InputClosed,
    #[error("tool was cancelled")]
    Cancelled,
    #[error("tool was interrupted")]
    Interrupted,
    #[error("tool failed: {0}")]
    Failed(String),
    #[error("tool failed: {message}")]
    FailedWithOutput { message: String, output: Box<O> },
    #[error("tool I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

impl<O: std::fmt::Debug> OperationError<O> {
    pub(crate) fn invalid(error: impl std::fmt::Display) -> Self {
        Self::InvalidArguments(error.to_string())
    }

    pub(crate) fn failed(error: impl std::fmt::Display) -> Self {
        Self::Failed(error.to_string())
    }

    pub(crate) fn concise_message(&self) -> String {
        match self {
            Self::Failed(message) | Self::FailedWithOutput { message, .. } => message.clone(),
            _ => self.to_string(),
        }
    }

    #[must_use]
    pub fn with_output(message: impl Into<String>, output: O) -> Self {
        Self::FailedWithOutput {
            message: message.into(),
            output: Box::new(output),
        }
    }
}

impl<O> OperationError<O> {
    pub(crate) fn try_map_output<P, E>(
        self,
        convert: impl FnOnce(O) -> Result<P, E>,
    ) -> Result<OperationError<P>, E> {
        Ok(match self {
            Self::Denied(value) => OperationError::Denied(value),
            Self::ArgumentsMustBeObject => OperationError::ArgumentsMustBeObject,
            Self::InvalidBackground => OperationError::InvalidBackground,
            Self::BackgroundUnsupported(value) => OperationError::BackgroundUnsupported(value),
            Self::InvalidArguments(value) => OperationError::InvalidArguments(value),
            Self::InputClosed => OperationError::InputClosed,
            Self::Cancelled => OperationError::Cancelled,
            Self::Interrupted => OperationError::Interrupted,
            Self::Failed(value) => OperationError::Failed(value),
            Self::FailedWithOutput { message, output } => OperationError::FailedWithOutput {
                message,
                output: Box::new(convert(*output)?),
            },
            Self::Io(error) => OperationError::Io(error),
            Self::Json(error) => OperationError::Json(error),
        })
    }
}

#[derive(Debug, Error)]
pub enum AdmissionError {
    #[error("operation denied: {0}")]
    Denied(String),
    #[error("tool arguments must be a JSON object")]
    ArgumentsMustBeObject,
    #[error("`bg` must be a boolean")]
    InvalidBackground,
    #[error("tool `{0}` does not support background execution")]
    BackgroundUnsupported(String),
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("tool was cancelled")]
    Cancelled,
    #[error("tool failed: {0}")]
    Failed(String),
    #[error("tool I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl AdmissionError {
    pub(crate) fn invalid(error: impl std::fmt::Display) -> Self {
        Self::InvalidArguments(error.to_string())
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
                tool.admit(arguments)?
                    .call(context.with_arguments(original_arguments))
                    .await
            }
            PathOutcome::ReadError(value) => Ok(ProducedOutput::new(value)),
        }
    }
}

pub(crate) struct PathPreflight {
    pub(crate) permissions: Vec<PermissionUse>,
    pub(crate) outcome: PathOutcome,
}

pub(crate) enum PathOutcome {
    Ready,
    ReadError(Value),
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
    let mut outcome = PathOutcome::Ready;
    for spec in tool.path_arguments(arguments)? {
        let Some(input) = spec.input(arguments)?.map(str::to_owned) else {
            continue;
        };
        let resolved = match resolve_for_authorization(workspace, &input, spec.kind).await {
            Ok(resolved) => resolved,
            Err(error) => {
                let Some(output) = tool.read_error_output(&input, &error) else {
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
                outcome = PathOutcome::ReadError(output);
                continue;
            }
        };
        // Both the existing handler JSON and permission resource are Unicode
        // boundaries. Never authorize a replacement-character alias.
        let value = Value::String(path_text(&resolved.path)?.to_owned());
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
