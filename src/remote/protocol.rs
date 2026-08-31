use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::provider::protocol::{
    AssistantContent, Message, ModelRequest, ResponseChunk, SystemSegment, ToolCall,
    ToolDefinition, ToolResult, Usage,
};
use crate::tool::builtins::ProcessOutput;
use crate::{media::ImageReference, tool::ToolOutput};
use serde_json::Value;

pub const PROTOCOL_VERSION: u16 = 1;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        version: u16,
    },
    Execute(ProcessRequest),
    Tool {
        name: String,
        arguments: Value,
    },
    AgentStart {
        id: String,
        spec: RemoteAgentSpec,
    },
    AgentProvider {
        id: String,
        message: Option<Message>,
        chunks: Vec<ResponseChunk>,
    },
    AgentTools {
        id: String,
        results: Vec<ToolResult>,
    },
    AgentAbort {
        id: String,
    },
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello {
        version: u16,
        arch: String,
        os: String,
    },
    Process {
        result: Result<ProcessOutput, String>,
    },
    Tool {
        result: Result<RemoteToolOutput, String>,
    },
    Agent {
        result: Result<RemoteAgentStep, String>,
    },
    Accepted,
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RemoteToolOutput {
    pub value: Value,
    pub images: Vec<RemoteImage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RemoteImage {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RemoteAgentSpec {
    pub model: String,
    pub provider: String,
    pub system: Vec<SystemSegment>,
    pub tools: Vec<ToolDefinition>,
    pub reasoning: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub history: Vec<Message>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RemoteClock {
    pub date: String,
    pub timezone: String,
    pub utc_offset: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteAgentStep {
    Started {
        request: ModelRequest,
        clock: RemoteClock,
    },
    Provider {
        request: ModelRequest,
    },
    Tools {
        blocks: Vec<AssistantContent>,
        usage: Usage,
        calls: Vec<ToolCall>,
    },
    Complete {
        blocks: Vec<AssistantContent>,
        usage: Usage,
        text: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessRequest {
    Exec {
        argv: Vec<String>,
        cwd: String,
        timeout: u64,
    },
    Shell {
        command: String,
        cwd: String,
        timeout: u64,
    },
}

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
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

pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>, std::io::Error>
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
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut left, mut right) = tokio::io::duplex(4096);
        let sent = Request::Hello {
            version: PROTOCOL_VERSION,
        };
        let write = tokio::spawn(async move { write_frame(&mut left, &sent).await.unwrap() });
        let received: Request = read_frame(&mut right).await.unwrap().unwrap();
        write.await.unwrap();
        assert!(matches!(
            received,
            Request::Hello {
                version: PROTOCOL_VERSION
            }
        ));
    }
}
