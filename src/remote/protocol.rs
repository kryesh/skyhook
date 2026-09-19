use std::num::NonZeroU64;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::{
    media::{Image, MAX_IMAGE_BYTES, decode_base64_bounded},
    session::SessionStore,
    tool::{
        ToolOutput,
        policy::{Capability, PermissionUse},
    },
};
use serde_json::Value;

// Version 3 sends tool images as source-format bytes with their file provenance.
// Version 4 removes shim-side SSH configuration resolution.
pub(crate) const PROTOCOL_VERSION: u32 = 4;

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
    SensitiveAnswer {
        prompt_id: PromptId,
        answer: super::prompt::PromptAnswer,
    },
    Hello {
        version: u32,
    },
    Tool {
        request_id: RequestId,
        name: String,
        arguments: Value,
        capabilities: Vec<Capability>,
    },
    // Best effort only: the protocol has no cancel acknowledgment. The host keeps the
    // request registered for late artifacts/chunks until its terminal Tool reply
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
    ToolArtifact {
        request_id: RequestId,
        field: String,
        #[serde(default)]
        kind: crate::job::output::CaptureKind,
        offset: u64,
        data: Vec<u8>,
        finished: bool,
    },
    ToolChunk {
        request_id: RequestId,
        offset: u64,
        data: Vec<u8>,
        finished: bool,
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
    Ready {
        version: u32,
    },
    Tool {
        request_id: RequestId,
        result: Result<RemoteToolOutput, RemoteToolError>,
    },
    Authorization {
        request_id: RequestId,
        authorization_id: AuthorizationId,
        tool: String,
        permissions: Vec<PermissionUse>,
        arguments: Value,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteToolOutput {
    pub value: Value,
    pub images: Vec<RemoteImage>,
    // Required: a peer that omits it must not have cut output recorded as finished.
    pub streams: crate::tool::StreamEnd,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteToolError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial: Option<crate::tool::Denial>,
    pub output: Option<Box<RemoteToolOutput>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteImage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub data_base64: String,
}

impl RemoteToolOutput {
    /// Decode each bounded wire image and store it, yielding a local tool output.
    /// The value, stream-end marker and every valid image survive: an image that
    /// fails to decode or store is dropped on its own rather than costing the
    /// call its textual result.
    pub(crate) async fn store(self, store: &SessionStore) -> ToolOutput {
        let mut images = Vec::with_capacity(self.images.len());
        for image in self.images {
            let Ok(bytes) = decode_base64_bounded(&image.data_base64, MAX_IMAGE_BYTES as usize)
            else {
                continue;
            };
            let Ok(decoded) = Image::new(bytes) else {
                continue;
            };
            if let Ok(stored) = store.store_image(image.file, &decoded).await {
                images.push(stored);
            }
        }
        let mut output = ToolOutput::new(self.value).with_images(images);
        output.streams = self.streams;
        output
    }
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

/// Send completed results as bounded frames, never one unbounded RPC message.
pub(crate) async fn write_tool_result<W: AsyncWrite + Unpin>(
    writer: &tokio::sync::Mutex<W>,
    request_id: RequestId,
    result: &Result<RemoteToolOutput, RemoteToolError>,
) -> std::io::Result<()> {
    use std::io::{Seek as _, Write as _};
    let mut file = tempfile::spooled_tempfile(64 * 1024);
    {
        let mut buffered = std::io::BufWriter::new(&mut file);
        serde_json::to_writer(&mut buffered, result)?;
        buffered.flush()?;
    }
    file.rewind()?;
    if !file.is_rolled() {
        // Borrow the original result: no disk I/O, deserialization, or payload clone.
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum BorrowedResponse<'a> {
            Tool {
                request_id: RequestId,
                result: &'a Result<RemoteToolOutput, RemoteToolError>,
            },
        }
        return write_frame(
            &mut *writer.lock().await,
            &BorrowedResponse::Tool { request_id, result },
        )
        .await;
    }
    let frame = move |offset, data, finished| Response::ToolChunk {
        request_id,
        offset,
        data,
        finished,
    };
    write_chunks(writer, file.into_file()?, frame).await
}

pub(crate) async fn write_artifact<W: AsyncWrite + Unpin>(
    writer: &tokio::sync::Mutex<W>,
    request_id: RequestId,
    field: String,
    kind: crate::job::output::CaptureKind,
    input: crate::job::output::Source,
) -> std::io::Result<()> {
    let frame = move |offset, data, finished| Response::ToolArtifact {
        request_id,
        field: field.clone(),
        kind,
        offset,
        data,
        finished,
    };
    write_chunks(writer, input, frame).await
}

/// Sends `input` as 64 KiB frames followed by an empty `finished` frame.
async fn write_chunks<W: AsyncWrite + Unpin, R: std::io::Read + Send + 'static>(
    writer: &tokio::sync::Mutex<W>,
    mut input: R,
    frame: impl Fn(u64, Vec<u8>, bool) -> Response,
) -> std::io::Result<()> {
    let mut buffer = vec![0; 64 * 1024];
    let mut offset = 0;
    loop {
        // Capture reads query the session database; keep them off the runtime.
        let length;
        (input, buffer, length) = tokio::task::spawn_blocking(move || {
            let length = input.read(&mut buffer);
            (input, buffer, length)
        })
        .await
        .map_err(std::io::Error::other)?;
        let length = length?;
        let response = frame(offset, buffer[..length].to_vec(), length == 0);
        write_frame(&mut *writer.lock().await, &response).await?;
        if length == 0 {
            return Ok(());
        }
        offset += length as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;

    #[tokio::test]
    async fn remote_images_are_bounded_decoded_images_before_storage() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create(directory.path()).await.unwrap();
        let png = crate::tests::png(b"remote image");
        let output = |data_base64| RemoteToolOutput {
            streams: Default::default(),
            value: json!({}),
            images: vec![RemoteImage {
                file: Some("fixture.png".into()),
                data_base64,
            }],
        };
        let stored = output(STANDARD.encode(png.bytes())).store(&store).await;
        let [stored] = stored.images.as_slice() else {
            panic!("expected one stored image")
        };
        assert_eq!(stored.file.as_deref(), Some("fixture.png"));
        assert_eq!(stored.format, png.format());
        assert_eq!(stored.blob, crate::media::BlobRef::of(png.bytes()));
        let bytes = store
            .read_blob(&stored.blob, MAX_IMAGE_BYTES as usize)
            .await;
        assert_eq!(bytes.unwrap(), png.bytes());
        let oversized = STANDARD.encode(vec![0; MAX_IMAGE_BYTES as usize + 1]);
        for invalid in [
            "%%%".to_owned(),
            STANDARD.encode(b"not an image"),
            oversized,
        ] {
            assert!(output(invalid).store(&store).await.images.is_empty());
        }
    }

    #[test]
    fn wire_ids_capabilities_and_handshakes_are_exact() {
        // IDs remain numbers and only request IDs reject zero.
        let wire = json!({"type":"cancel", "request_id":1});
        let request: Request = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), wire);
        assert!(serde_json::from_value::<RequestId>(json!(0)).is_err());
        assert_eq!(serde_json::to_value(AuthorizationId(0)).unwrap(), 0);
        // Tool capabilities are required and an empty list is exact.
        let mut request = json!({"type": "tool", "request_id": 1, "name": "exec", "arguments": {}});
        assert!(serde_json::from_value::<Request>(request.clone()).is_err());
        request["capabilities"] = json!([]);
        let decoded = serde_json::from_value::<Request>(request.clone()).unwrap();
        assert!(matches!(&decoded, Request::Tool { capabilities, .. } if capabilities.is_empty()));
        assert_eq!(serde_json::to_value(decoded).unwrap(), request);
        // Tool outputs must state whether their streams ran to the end.
        let mut output =
            json!({"type":"tool","request_id":1,"result":{"Ok":{"value":{},"images":[]}}});
        assert!(serde_json::from_value::<Response>(output.clone()).is_err());
        output["result"]["Ok"]["streams"] = json!("cut");
        let decoded = serde_json::from_value::<Response>(output.clone()).unwrap();
        assert!(matches!(
            &decoded,
            Response::Tool {
                result: Ok(RemoteToolOutput {
                    streams: crate::tool::StreamEnd::Cut,
                    ..
                }),
                ..
            }
        ));
        assert_eq!(serde_json::to_value(decoded).unwrap(), output);
        // Unversioned handshakes are rejected.
        assert!(serde_json::from_value::<Request>(json!({"type":"hello"})).is_err());
        assert!(serde_json::from_value::<Response>(json!({"type":"ready"})).is_err());
    }

    #[tokio::test]
    async fn tool_results_round_trip_in_small_and_spilled_forms() {
        const ID: RequestId = RequestId::new(7).unwrap();
        for length in [0, 70 * 1024] {
            let value = "x".repeat(length);
            let result = Err(RemoteToolError {
                message: "timed out".into(),
                denial: None,
                output: Some(Box::new(RemoteToolOutput {
                    streams: Default::default(),
                    value: json!({ "stdout": value }),
                    images: Vec::new(),
                })),
            });
            let (writer, mut reader) = tokio::io::duplex(4096);
            let send = tokio::spawn(async move {
                let writer = tokio::sync::Mutex::new(writer);
                write_tool_result(&writer, ID, &result).await.unwrap();
            });
            let mut bytes = Vec::new();
            let received = loop {
                match read_frame::<_, Response>(&mut reader)
                    .await
                    .unwrap()
                    .unwrap()
                {
                    Response::Tool {
                        request_id: ID,
                        result,
                    } => {
                        assert!(length < 64 * 1024);
                        break result;
                    }
                    Response::ToolChunk {
                        request_id: ID,
                        offset,
                        data,
                        finished,
                    } => {
                        assert_eq!(offset, bytes.len() as u64);
                        assert!(data.len() <= 64 * 1024);
                        if finished {
                            assert!(data.is_empty());
                            break serde_json::from_slice::<
                                Result<RemoteToolOutput, RemoteToolError>,
                            >(&bytes)
                            .unwrap();
                        }
                        bytes.extend(data);
                    }
                    frame => panic!("unexpected result frame: {frame:?}"),
                }
            };
            assert_eq!(received.unwrap_err().output.unwrap().value["stdout"], value);
            send.await.unwrap();
        }
    }
}
