use std::{
    marker::PhantomData,
    num::NonZeroU64,
    sync::{Mutex, PoisonError},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::{
    target::Route,
    tool::{
        authorization::AuthorizationError,
        diagnostic::{Diagnostic, PartialDiagnostic},
        output::{CaptureEvent, CaptureId, ProducedOutput},
        policy::Capability,
    },
};
use serde_json::Value;

pub(super) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

macro_rules! wire_ids {
    ($($(#[$attr:meta])* $name:ident),+ $(,)?) => {$(
        $(#[$attr])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
        #[serde(transparent)]
        pub(crate) struct $name(NonZeroU64);

        impl From<NonZeroU64> for $name {
            fn from(id: NonZeroU64) -> Self {
                Self(id)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        #[cfg(test)]
        impl $name {
            pub(crate) const fn new(id: u64) -> Self {
                Self(NonZeroU64::new(id).unwrap())
            }
        }
    )+};
}

wire_ids! {
    /// Requests and SSH channels share one connection's sequence.
    RequestId,
    /// Scoped to its request.
    AuthorizationId,
    PromptId,
    /// Scoped to its request.
    ImageId,
}

/// Allocates IDs of one kind in order from 1; once exhausted it yields none.
pub(crate) struct Sequence<T>(Mutex<Option<NonZeroU64>>, PhantomData<fn() -> T>);

impl<T> Default for Sequence<T> {
    fn default() -> Self {
        Self(Mutex::new(Some(NonZeroU64::MIN)), PhantomData)
    }
}

impl<T: From<NonZeroU64>> Sequence<T> {
    pub(crate) fn next(&self) -> Option<T> {
        let mut next = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let id = (*next)?;
        *next = id.checked_add(1);
        Some(T::from(id))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Request {
    Control(ControlRequest),
    PayloadAck,
    Hello,
    Tool {
        request_id: RequestId,
        name: String,
        arguments: Value,
        capabilities: Vec<Capability>,
        /// The call has a source argument, whose contents follow as
        /// `SourceData` frames ending with `SourceEnd` before the call starts.
        source: bool,
    },
    /// At most one flow-control window of chunks is unacknowledged by `SourceAck`.
    SourceData {
        request_id: RequestId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    SourceEnd {
        request_id: RequestId,
    },
    /// Read a source file's bytes on this machine for a call running elsewhere.
    ReadSource {
        request_id: RequestId,
        /// The consuming tool, which the read is authorized and reported as.
        tool: String,
        path: String,
        capabilities: Vec<Capability>,
    },
    // Best effort only: the protocol has no cancel acknowledgment. The host keeps the
    // request registered for late payloads until its terminal Tool reply
    // (or connection failure); local abandonment is not a wire terminal event.
    Cancel {
        request_id: RequestId,
    },
    AuthorizationDecision {
        request_id: RequestId,
        authorization_id: AuthorizationId,
        decision: AuthorizationDecision,
    },
}

/// Requests for the worker's private SSH and prompt services.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ControlRequest {
    OpenSsh {
        channel: RequestId,
        route: Box<Route>,
        command: String,
    },
    StreamData {
        channel: RequestId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    StreamEnd {
        channel: RequestId,
    },
    StreamAck {
        channel: RequestId,
    },
    StreamClose {
        channel: RequestId,
    },
    SensitiveAnswer {
        prompt_id: PromptId,
        answer: super::prompt::PromptAnswer,
    },
}

/// The host's answer to a shim's authorization request.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum AuthorizationDecision {
    Allowed,
    Denied(String),
    InvalidGrant(String),
    Cancelled,
    Unavailable(String),
    /// The host policy stopped before deciding.
    Failed,
}

impl From<Result<(), AuthorizationError>> for AuthorizationDecision {
    fn from(decision: Result<(), AuthorizationError>) -> Self {
        match decision {
            Ok(()) => Self::Allowed,
            Err(AuthorizationError::Denied(reason)) => Self::Denied(reason),
            Err(AuthorizationError::InvalidGrant(reason)) => Self::InvalidGrant(reason),
            Err(AuthorizationError::Cancelled) => Self::Cancelled,
            Err(AuthorizationError::Unavailable(tool)) => Self::Unavailable(tool),
            Err(AuthorizationError::PolicyFailed) => Self::Failed,
        }
    }
}

impl AuthorizationDecision {
    pub(crate) fn into_result(self) -> Result<(), AuthorizationError> {
        match self {
            Self::Allowed => Ok(()),
            Self::Denied(reason) => Err(AuthorizationError::Denied(reason)),
            Self::InvalidGrant(reason) => Err(AuthorizationError::InvalidGrant(reason)),
            Self::Cancelled => Err(AuthorizationError::Cancelled),
            Self::Unavailable(tool) => Err(AuthorizationError::Unavailable(tool)),
            Self::Failed => Err(AuthorizationError::PolicyFailed),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Response {
    Payload {
        request_id: RequestId,
        event: PayloadEvent,
    },
    SensitiveCancelled {
        prompt_id: PromptId,
    },
    StreamData {
        channel: RequestId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    StreamClosed {
        channel: RequestId,
        error: Option<Diagnostic>,
    },
    StreamAck {
        channel: RequestId,
    },
    /// The worker spooled one `SourceData` chunk of this request's upload.
    SourceAck {
        request_id: RequestId,
    },
    SensitivePrompt {
        prompt_id: PromptId,
        prompt: super::SensitivePrompt,
    },
    Ready,
    Tool {
        request_id: RequestId,
    },
    Authorization {
        request_id: RequestId,
        authorization_id: AuthorizationId,
        request: crate::tool::authorization::Reauthorization,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub(crate) enum PayloadId {
    Image(ImageId),
    /// The contents of a `ReadSource` request's file.
    Source,
    Result,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum PayloadOpen {
    Image { id: ImageId, file: Option<String> },
    Source,
    Result,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum PayloadEvent {
    Capture(CaptureEvent),
    Open(PayloadOpen),
    Data {
        id: PayloadId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    Finish {
        id: PayloadId,
    },
}

pub(crate) type RemoteToolResult = Result<RemoteToolOutput, RemoteToolError>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteToolOutput {
    pub value: Value,
    pub images: Vec<crate::media::ImageRef>,
    pub captures: Vec<CaptureId>,
    pub streams: crate::tool::StreamEnd,
    pub diagnostic: Option<Diagnostic>,
}

impl From<ProducedOutput> for RemoteToolOutput {
    fn from(output: ProducedOutput) -> Self {
        Self {
            value: output.value,
            images: output.images,
            captures: output
                .captures
                .into_iter()
                .map(|capture| capture.id())
                .collect(),
            streams: output.streams,
            diagnostic: output.diagnostic.map(PartialDiagnostic::resolve),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteToolError {
    pub diagnostic: Box<Diagnostic>,
    pub output: Option<Box<RemoteToolOutput>>,
}

pub(crate) async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    write_frame_within(writer, value, MAX_FRAME_BYTES).await
}

/// Write a frame of at most `limit` bytes.
pub(super) async fn write_frame_within<W, T>(
    writer: &mut W,
    value: &T,
    limit: usize,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = zeroize::Zeroizing::new(
        rmp_serde::to_vec_named(value)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
    );
    if bytes.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "RPC frame is too large",
        ));
    }
    let length = u32::try_from(bytes.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "RPC frame is too large")
    })?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

/// Write one whole frame on its own task: dropping the caller mid-write would
/// otherwise leave a partial frame on the wire. The guard travels with the task
/// so the writer stays exclusively owned until the frame is finished.
pub(crate) async fn spawn_owned_write<G, W, T>(mut writer: G, frame: T) -> std::io::Result<()>
where
    G: std::ops::DerefMut<Target = W> + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    T: Serialize + Send + Sync + 'static,
{
    tokio::spawn(async move { write_frame(&mut *writer, &frame).await })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
}

pub(crate) async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>, std::io::Error>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length = [0_u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = usize::try_from(u32::from_be_bytes(length)).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid RPC frame length")
    })?;
    if length > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "RPC frame is too large",
        ));
    }
    let mut bytes = zeroize::Zeroizing::new(vec![0; length]);
    reader.read_exact(&mut bytes).await?;
    rmp_serde::from_slice(&bytes)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{AdmissionError, diagnostic::Cause};

    /// Every path that reports an authorization failure to a tool reads it the
    /// same way, including after a wire round trip; a cancellation is never
    /// relayed as a denial.
    #[test]
    fn authorization_failures_read_the_same_on_every_path() {
        let cases = [
            (
                AuthorizationError::Denied("no".into()),
                Cause::Denied("no".into()),
            ),
            (AuthorizationError::Cancelled, Cause::Cancelled),
            (
                AuthorizationError::InvalidGrant("unproposed".into()),
                Cause::Message("unproposed".into()),
            ),
            (
                AuthorizationError::Unavailable("tool".into()),
                Cause::Unavailable {
                    tool: "tool".into(),
                },
            ),
            (
                AuthorizationError::PolicyFailed,
                Cause::Message("authorization could not be decided".into()),
            ),
        ];
        for (error, cause) in cases {
            let direct = AdmissionError::from(error.clone()).diagnostic().cause;
            let relayed = AuthorizationDecision::from(Err(error))
                .into_result()
                .map_err(AdmissionError::from)
                .unwrap_err()
                .diagnostic()
                .cause;
            assert_eq!(direct, cause);
            assert_eq!(relayed, cause);
        }
    }
}
