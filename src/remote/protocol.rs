use std::num::NonZeroU64;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::{
    tool::output::{CaptureEvent, CaptureId, ProducedOutput},
    tool::policy::{Capability, PermissionUse},
};
use serde_json::Value;

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Request and SSH channel IDs share one connection-local allocator. Zero is invalid.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct RequestId(NonZeroU64);

impl RequestId {
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }

    pub fn next(self) -> Option<Self> {
        self.get().checked_add(1).and_then(Self::new)
    }
}

/// Authorization IDs are scoped to requests; zero is a valid wire value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct AuthorizationId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct PromptId(pub u64);

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Request {
    OpenSsh {
        channel: RequestId,
        route: Vec<crate::target::TargetDefinition>,
        command: String,
    },
    StreamData {
        channel: RequestId,
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
    PayloadAck,
    SensitiveAnswer {
        prompt_id: PromptId,
        answer: super::prompt::PromptAnswer,
    },
    Hello,
    Tool {
        request_id: RequestId,
        name: String,
        arguments: Value,
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
        allowed: bool,
        reason: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
        data: Vec<u8>,
    },
    StreamClosed {
        channel: RequestId,
        error: Option<String>,
    },
    StreamAck {
        channel: RequestId,
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
        tool: String,
        permissions: Vec<PermissionUse>,
        arguments: Value,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct ImageId(pub NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub(crate) enum PayloadId {
    Image(ImageId),
    Result,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum PayloadOpen {
    Image { id: ImageId, file: Option<String> },
    Result,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum PayloadEvent {
    Capture(CaptureEvent),
    Open(PayloadOpen),
    Data { id: PayloadId, data: Vec<u8> },
    Finish { id: PayloadId },
}

pub(crate) type RemoteToolResult = Result<RemoteToolOutput, RemoteToolError>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteToolOutput {
    pub value: Value,
    pub images: Vec<crate::media::ImageRef>,
    pub captures: Vec<CaptureId>,
    pub streams: crate::tool::StreamEnd,
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
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteToolError {
    pub message: String,
    pub denial: Option<crate::tool::Denial>,
    pub output: Option<Box<RemoteToolOutput>>,
}

pub(crate) async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = zeroize::Zeroizing::new(serde_json::to_vec(value).map_err(std::io::Error::other)?);
    if bytes.len() > MAX_FRAME_BYTES {
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
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(std::io::Error::other)
}
