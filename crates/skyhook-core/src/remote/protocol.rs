use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::{
    media::ImageReference,
    tool::{ToolOutput, policy::PermissionUse},
};
use serde_json::Value;

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Request {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Response {
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub console_output: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RemoteToolError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial: Option<crate::tool::Denial>,
    pub output: Option<RemoteToolOutput>,
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
            console_output: value.console_output,
            images,
        }
    }
}

pub(crate) async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), std::io::Error>
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
                output: Some(RemoteToolOutput {
                    value: serde_json::json!({"stdout":"partial"}),
                    console_output: String::new(),
                    images: Vec::new(),
                }),
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
                    output: Some(RemoteToolOutput { value, .. }),
                    ..
                })
            } if value["stdout"] == "partial"
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
