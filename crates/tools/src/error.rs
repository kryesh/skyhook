//! How tool operations fail.

use std::io;

use crate::diagnostic::{
    Cause, ConnectionFault, Diagnostic, FailureSite, PartialContext, PartialDiagnostic,
    RequestEffects, safe_text,
};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};

/// A failure normalized at an annotation, transport, or persistence boundary.
/// Boxed so every handler `Result` stays pointer-sized.
#[derive(Clone, Debug)]
pub struct OperationError<O>(Box<Facts<O>>);

#[derive(Clone, Debug)]
struct Facts<O> {
    diagnostic: PartialDiagnostic,
    output: Option<O>,
}

/// Admission never produces partial output.
pub type AdmissionError = OperationError<std::convert::Infallible>;

impl<O> From<io::Error> for OperationError<O> {
    fn from(error: io::Error) -> Self {
        Self::io(error)
    }
}
impl<O> From<std::convert::Infallible> for OperationError<O> {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}
/// A regular file that could not be opened, read or removed.
impl<O> From<skyhook_worker::op::FsError> for OperationError<O> {
    fn from(error: skyhook_worker::op::FsError) -> Self {
        skyhook_worker::diagnostic::Failure::from(skyhook_worker::diagnostic::Fault::from(error))
            .into()
    }
}
/// A target's failure: its fault rendered, what failed filled where only the tool names it.
impl<O> From<skyhook_worker::diagnostic::Failure> for OperationError<O> {
    fn from(failure: skyhook_worker::diagnostic::Failure) -> Self {
        Self::from_facts(failure.into(), None)
    }
}
impl<O> From<skyhook_worker::op::OsError> for OperationError<O> {
    fn from(error: skyhook_worker::op::OsError) -> Self {
        Self::cause(Cause::os(error))
    }
}
impl<O> From<tokio::task::JoinError> for OperationError<O> {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::failed(error)
    }
}
impl<O> From<serde_json::Error> for OperationError<O> {
    fn from(_: serde_json::Error) -> Self {
        Self::cause(Cause::Json)
    }
}
/// Admission fails before any output exists, so it fails any call.
impl<C> From<AdmissionError> for OperationError<crate::output::Output<C>> {
    fn from(error: AdmissionError) -> Self {
        error.widen()
    }
}
impl AdmissionError {
    /// As the failure of a call of any output, which has none.
    pub fn widen<O>(self) -> OperationError<O> {
        let Facts { diagnostic, output } = *self.0;
        OperationError(Box::new(Facts {
            diagnostic,
            output: output.map(|never| match never {}),
        }))
    }
}
impl<O: std::fmt::Debug> std::error::Error for OperationError<O> {}
impl<O> std::fmt::Display for OperationError<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            &self
                .diagnostic()
                .render(crate::diagnostic::TargetView::Hidden),
        )
    }
}

impl<O> OperationError<O> {
    pub fn cause(cause: Cause) -> Self {
        Self::from_facts(
            PartialDiagnostic::new(PartialContext::default(), cause),
            None,
        )
    }
    pub fn denied(reason: impl Into<String>) -> Self {
        Self::cause(Cause::Denied(reason.into()))
    }
    pub fn unavailable(tool: &str) -> Self {
        Self::cause(Cause::Unavailable {
            tool: tool.to_owned(),
        })
    }
    pub fn unknown_tool(tool: &str) -> Self {
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
    /// A target could not be reached, or its connection failed, as `error` reads.
    pub fn connection(fault: ConnectionFault, error: impl std::fmt::Display) -> Self {
        let detail = safe_text(&error.to_string());
        Self::cause(Cause::Connection { fault, detail })
    }
    pub fn cancelled() -> Self {
        Self::cause(Cause::Cancelled)
    }
    /// Cancelled before the operation started, so nothing it would send was sent.
    pub fn cancelled_before_start() -> Self {
        Self::cancelled().effects(Effects::NotStarted)
    }
    pub fn interrupted() -> Self {
        Self::cause(Cause::Interrupted)
    }
    pub fn input_closed() -> Self {
        Self::cause(Cause::InputClosed)
    }
    /// An I/O failure; one of a spill as the fault it failed with.
    pub fn io(error: io::Error) -> Self {
        match skyhook_worker::fs::spill::SpillError::of(&error) {
            Some(spill) => skyhook_worker::diagnostic::Failure::from(spill.fault()).into(),
            None => Self::cause(Cause::io(&error)),
        }
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
    #[must_use]
    pub fn request(mut self, request: RequestEffects) -> Self {
        self.0.diagnostic.context = self.0.diagnostic.context.request(request);
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
    pub fn facts(&self) -> &PartialDiagnostic {
        &self.0.diagnostic
    }
    /// Extract the one authoritative diagnostic and any completion evidence.
    pub fn into_parts(self) -> (Diagnostic, Option<O>) {
        let (diagnostic, output) = self.into_facts();
        (diagnostic.resolve(), output)
    }
    pub fn into_facts(self) -> (PartialDiagnostic, Option<O>) {
        let Facts { diagnostic, output } = *self.0;
        (diagnostic, output)
    }
    /// Restore a resolved diagnostic; every fact it carries counts as chosen.
    pub fn from_diagnostic(diagnostic: Diagnostic, output: Option<O>) -> Self {
        Self::from_facts(diagnostic.into(), output)
    }
    pub fn from_facts(diagnostic: PartialDiagnostic, output: Option<O>) -> Self {
        Self(Box::new(Facts { diagnostic, output }))
    }
    pub fn try_map_output<P, E>(
        self,
        convert: impl FnOnce(O) -> Result<P, E>,
    ) -> Result<OperationError<P>, E> {
        let Facts { diagnostic, output } = *self.0;
        Ok(OperationError(Box::new(Facts {
            diagnostic,
            output: output.map(convert).transpose()?,
        })))
    }
}

pub type CallError = OperationError<crate::output::ProducedOutput>;

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use skyhook_worker::op::IoKind;

    #[test]
    fn normalization_retains_facts_and_output() {
        let context =
            PartialContext::new(Operation::Deserialize, Subject::argument(["items", "0"]))
                .at(FailureSite::Host)
                .effects(Effects::OutputIncomplete);
        let wire = Diagnostic::new(
            PartialContext::default().resolve(),
            Cause::Io(skyhook_worker::op::OsError {
                kind: IoKind::PermissionDenied,
                code: Some(2),
                detail: Some("remote reason".into()),
            }),
        );
        let native = serde_json::from_str::<serde_json::Value>("{private-payload").unwrap_err();
        for (error, cause) in [
            (OperationError::from(native), Cause::Json),
            (
                OperationError::io(io::Error::other("filesystem reason")),
                Cause::io(&io::Error::other("filesystem reason")),
            ),
            (
                OperationError::from_diagnostic(wire.clone(), None),
                wire.cause,
            ),
        ] {
            let error = error
                .with_result(1_u8)
                .context(context.clone())
                .with_result(7)
                .try_map_output(|value| Ok::<_, ()>(u16::from(value)))
                .unwrap();
            let (diagnostic, output) = error.into_parts();
            assert_eq!(diagnostic.cause, cause);
            assert_eq!(diagnostic.context, context.clone().resolve());
            assert_eq!(output, Some(7_u16));
            // Every fact of a restored diagnostic counts as chosen.
            let decoded =
                serde_json::from_value(serde_json::to_value(&diagnostic).unwrap()).unwrap();
            let restored = OperationError::from_diagnostic(decoded, output).or(
                PartialContext::new(Operation::Write, Subject::path("fallback")),
            );
            assert_eq!(restored.into_parts(), (diagnostic, output));
        }
    }
}
