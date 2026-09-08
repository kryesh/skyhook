use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::{
    media::ImageReference,
    tool::{ToolOutput, policy::PermissionUse},
};
use serde_json::Value;

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
        answer: super::askpass::PromptAnswer,
    },
    Hello,
    Tool {
        request_id: u64,
        name: String,
        arguments: Value,
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
    Ready,
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
    let mut file = tempfile::tempfile()?;
    {
        let mut buffered = std::io::BufWriter::new(&mut file);
        serde_json::to_writer(&mut buffered, result)?;
        buffered.flush()?;
    }
    file.rewind()?;
    if file.metadata()?.len() <= 64 * 1024 {
        let result = serde_json::from_reader(std::io::BufReader::new(file))?;
        return write_frame(
            &mut *writer.lock().await,
            &Response::Tool { request_id, result },
        )
        .await;
    }
    let mut file = tokio::fs::File::from_std(file);
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
    use super::*;

    #[test]
    fn remote_output_keeps_console_only_inside_script_result() {
        let output: RemoteToolOutput = serde_json::from_value(serde_json::json!({
            "value":{"value":42,"console":"captured\n"},
            "images":[]
        }))
        .unwrap();
        let serialized = serde_json::to_value(&output).unwrap();
        assert!(serialized.get("console_output").is_none());
        assert!(serialized.get("console").is_none());
        let native: ToolOutput = output.into();
        assert_eq!(
            native.value,
            serde_json::json!({"value":42,"console":"captured\n"})
        );
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut left, mut right) = tokio::io::duplex(4096);
        let sent = Request::Hello;
        let write = tokio::spawn(async move { write_frame(&mut left, &sent).await.unwrap() });
        let received: Request = read_frame(&mut right).await.unwrap().unwrap();
        write.await.unwrap();
        assert!(matches!(received, Request::Hello));

        let (mut left, mut right) = tokio::io::duplex(4096);
        let sent = Response::Tool {
            request_id: 7,
            result: Err(RemoteToolError {
                message: "timed out".to_owned(),
                denial: None,
                output: Some(Box::new(RemoteToolOutput {
                    value: serde_json::json!({"stdout":"partial"}),
                    images: Vec::new(),
                })),
            }),
        };
        let write = tokio::spawn(async move { write_frame(&mut left, &sent).await.unwrap() });
        let received: Response = read_frame(&mut right).await.unwrap().unwrap();
        write.await.unwrap();
        assert!(matches!(
            received,
            Response::Tool {
                request_id: 7,
                result: Err(RemoteToolError {
                    output: Some(output),
                    ..
                })
            } if output.value["stdout"] == "partial"
        ));

        let (mut left, mut right) = tokio::io::duplex(4096);
        let write = tokio::spawn(async move {
            write_frame(&mut left, &Request::Cancel { request_id: 9 })
                .await
                .unwrap();
        });
        assert!(matches!(
            read_frame::<_, Request>(&mut right).await.unwrap(),
            Some(Request::Cancel { request_id: 9 })
        ));
        write.await.unwrap();
    }
}
