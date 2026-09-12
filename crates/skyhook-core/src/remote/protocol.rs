use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::{
    media::ImageReference,
    tool::{
        ToolOutput,
        policy::{Capability, PermissionUse},
    },
};
use serde_json::Value;

// Version 2 requires exact originating capabilities on every tool request.
pub(crate) const PROTOCOL_VERSION: u32 = 2;

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Request {
    ResolveSsh {
        request_id: u64,
        target: Box<crate::target::TargetDefinition>,
    },
    OpenSsh {
        channel: u64,
        route: Vec<crate::target::TargetDefinition>,
        command: String,
    },
    StreamData {
        channel: u64,
        data: Vec<u8>,
    },
    StreamEnd {
        channel: u64,
    },
    StreamAck {
        channel: u64,
    },
    StreamClose {
        channel: u64,
    },
    SensitiveAnswer {
        prompt_id: u64,
        answer: super::prompt::PromptAnswer,
    },
    Hello {
        version: u32,
    },
    Tool {
        request_id: u64,
        name: String,
        arguments: Value,
        capabilities: Vec<Capability>,
    },
    Cancel {
        request_id: u64,
    },
    AuthorizationDecision {
        request_id: u64,
        authorization_id: u64,
        allowed: bool,
        reason: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Response {
    ToolArtifact {
        request_id: u64,
        field: String,
        #[serde(default)]
        kind: crate::job::output::CaptureKind,
        offset: u64,
        data: Vec<u8>,
        finished: bool,
    },
    ToolChunk {
        request_id: u64,
        offset: u64,
        data: Vec<u8>,
        finished: bool,
    },
    SensitiveCancelled {
        prompt_id: u64,
    },
    ResolvedSsh {
        request_id: u64,
        result: Result<super::ssh::ResolvedSsh, String>,
    },
    StreamData {
        channel: u64,
        data: Vec<u8>,
    },
    StreamClosed {
        channel: u64,
        error: Option<String>,
    },
    StreamAck {
        channel: u64,
    },
    SensitivePrompt {
        prompt_id: u64,
        prompt: super::SensitivePrompt,
    },
    Ready {
        version: u32,
    },
    Tool {
        request_id: u64,
        result: Result<RemoteToolOutput, RemoteToolError>,
    },
    Authorization {
        request_id: u64,
        authorization_id: u64,
        tool: String,
        permissions: Vec<PermissionUse>,
        arguments: Value,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteToolOutput {
    pub value: Value,
    pub images: Vec<RemoteImage>,
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
    pub reference: ImageReference,
    pub data_base64: String,
}

impl From<RemoteToolOutput> for ToolOutput {
    fn from(value: RemoteToolOutput) -> Self {
        let images = value
            .images
            .into_iter()
            .map(|image| {
                let mut reference = image.reference;
                reference.data_base64 = Some(image.data_base64);
                reference
            })
            .collect();
        Self {
            value: value.value,
            images,
        }
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
    request_id: u64,
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
                request_id: u64,
                result: &'a Result<RemoteToolOutput, RemoteToolError>,
            },
        }
        return write_frame(
            &mut *writer.lock().await,
            &BorrowedResponse::Tool { request_id, result },
        )
        .await;
    }
    let mut file = tokio::fs::File::from_std(file.into_file()?);
    let mut offset = 0;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let length = file.read(&mut buffer).await?;
        write_frame(
            &mut *writer.lock().await,
            &Response::ToolChunk {
                request_id,
                offset,
                data: buffer[..length].to_vec(),
                finished: length == 0,
            },
        )
        .await?;
        if length == 0 {
            return Ok(());
        }
        offset += length as u64;
    }
}

pub(crate) async fn write_artifact<W: AsyncWrite + Unpin>(
    writer: &tokio::sync::Mutex<W>,
    request_id: u64,
    field: String,
    kind: crate::job::output::CaptureKind,
    path: &std::path::Path,
) -> std::io::Result<()> {
    let mut input = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0; 64 * 1024];
    let mut offset = 0;
    loop {
        let length = input.read(&mut buffer).await?;
        write_frame(
            &mut *writer.lock().await,
            &Response::ToolArtifact {
                request_id,
                field: field.clone(),
                kind,
                offset,
                data: buffer[..length].to_vec(),
                finished: length == 0,
            },
        )
        .await?;
        if length == 0 {
            return Ok(());
        }
        offset += length as u64;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_capabilities_are_required_and_empty_is_exact() {
        let mut request = serde_json::json!({
            "type": "tool", "request_id": 1, "name": "exec", "arguments": {}
        });
        assert!(serde_json::from_value::<super::Request>(request.clone()).is_err());
        request["capabilities"] = serde_json::json!([]);
        let decoded = serde_json::from_value::<super::Request>(request.clone()).unwrap();
        assert!(
            matches!(&decoded, super::Request::Tool { capabilities, .. } if capabilities.is_empty())
        );
        assert_eq!(serde_json::to_value(decoded).unwrap(), request);
    }

    #[test]
    fn unversioned_handshakes_are_rejected() {
        assert!(
            serde_json::from_value::<super::Request>(serde_json::json!({"type":"hello"})).is_err()
        );
        assert!(
            serde_json::from_value::<super::Response>(serde_json::json!({"type":"ready"})).is_err()
        );
    }

    use super::*;

    #[tokio::test]
    async fn tool_results_round_trip_in_small_and_spilled_forms() {
        for length in [0, 70 * 1024] {
            let value = "x".repeat(length);
            let result = Err(RemoteToolError {
                message: "timed out".into(),
                denial: None,
                output: Some(Box::new(RemoteToolOutput {
                    value: serde_json::json!({"stdout":value}),
                    images: Vec::new(),
                })),
            });
            let (writer, mut reader) = tokio::io::duplex(4096);
            let send = tokio::spawn(async move {
                write_tool_result(&tokio::sync::Mutex::new(writer), 7, &result)
                    .await
                    .unwrap();
            });
            let mut bytes = Vec::new();
            let received = loop {
                match read_frame::<_, Response>(&mut reader)
                    .await
                    .unwrap()
                    .unwrap()
                {
                    Response::Tool {
                        request_id: 7,
                        result,
                    } => {
                        assert!(length < 64 * 1024);
                        break result;
                    }
                    Response::ToolChunk {
                        request_id: 7,
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
