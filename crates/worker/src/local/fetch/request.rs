//! Replayable request bodies and transport failures.
use std::error::Error;

use bytes::Bytes;
use futures_util::{Stream, TryStreamExt as _};
use reqwest::header::{HeaderMap, HeaderValue};
use tokio::io::AsyncReadExt;

use super::os;
use crate::diagnostic::{Failure, Fault};
use crate::op::{Events, FetchBody, FetchEvent, FetchStep, FsError, OsError, TargetPath};

const UPLOAD_CHUNK: usize = 64 * 1024;

pub(super) struct PreparedBody {
    bytes: Bytes,
    content_type: Option<HeaderValue>,
}

/// A file sent as a request body, with the length it had when the request was
/// prepared.
pub(super) struct FileUpload {
    path: TargetPath,
    length: u64,
}

impl FileUpload {
    async fn open(path: TargetPath, events: &Events<FetchEvent>) -> Result<Self, Fault> {
        super::step(events, FetchStep::ReadUpload(path.clone())).await;
        let opened = crate::fs::open_regular(path.native(), u64::MAX).await;
        let length = opened.map_err(upload)?.metadata().map_err(os)?.len();
        Ok(Self { path, length })
    }

    /// The file's first `length` bytes as it holds them when a hop sends it. Each
    /// hop reads the file itself, so one still sending shares nothing with the next.
    fn replay(&self) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
        let (path, length) = (self.path.clone(), self.length);
        let opened = async move {
            let file = crate::fs::open_regular(path.native(), u64::MAX).await;
            let file = file.map_err(|error| match error {
                FsError::Os(error) => std::io::Error::from(&error),
                error => std::io::Error::other(error),
            })?;
            let body = file.into_async().take(length);
            let body = tokio_util::io::ReaderStream::with_capacity(body, UPLOAD_CHUNK);
            Ok::<_, std::io::Error>(body)
        };
        futures_util::stream::once(opened).try_flatten()
    }
}

pub(super) enum Upload {
    Bytes(PreparedBody),
    File(FileUpload),
}

/// An upload file that failed its guards fails as the argument naming it.
fn upload(error: FsError) -> Fault {
    match error {
        FsError::NotRegular => Fault::UploadNotRegular,
        error => error.into(),
    }
}

pub(super) async fn prepare_body(
    body: Option<FetchBody>,
    events: &Events<FetchEvent>,
) -> Result<Option<Upload>, Failure> {
    Ok(Some(match body {
        None => return Ok(None),
        Some(FetchBody::File { path }) => Upload::File(FileUpload::open(path, events).await?),
        Some(FetchBody::Bytes {
            content_type,
            length,
            input,
        }) => {
            let mut bytes = Vec::new();
            (input
                .into_reader()
                .take(length)
                .read_to_end(&mut bytes)
                .await)
                .map_err(os)?;
            Upload::Bytes(PreparedBody {
                bytes: Bytes::from(bytes),
                content_type: content_type.map(|kind| HeaderValue::from_static(kind.as_header())),
            })
        }
    }))
}

pub(super) fn apply_body(
    request: reqwest::RequestBuilder,
    body: Option<&Upload>,
    headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    match body {
        None => request,
        Some(Upload::Bytes(body)) => match &body.content_type {
            Some(content_type) if !headers.contains_key("content-type") => {
                request.header("content-type", content_type)
            }
            _ => request,
        }
        .body(body.bytes.clone()),
        Some(Upload::File(data)) => (request.header("content-length", data.length))
            .body(reqwest::Body::wrap_stream(data.replay())),
    }
}

/// HTTP, TLS, resolver and I/O error displays can contain URLs, credentials,
/// response data or local paths, so none is copied: a transport failure carries
/// a typed I/O kind and OS code, or only its class.
pub(super) fn transport_error(error: reqwest::Error) -> Fault {
    // io::Error::source skips the error it wraps, so descend through get_ref.
    let chain =
        std::iter::successors(Some(&error as &(dyn Error + 'static)), |&error| match error
            .downcast_ref::<std::io::Error>(
        ) {
            Some(io) => io.get_ref().map(|inner| inner as &(dyn Error + 'static)),
            None => error.source(),
        });
    let mut io = None;
    for source in chain {
        if source.is::<rustls::Error>() {
            return Fault::Tls;
        }
        io = source.downcast_ref::<std::io::Error>().or(io);
    }
    if error.is_timeout() {
        return match error.is_connect() {
            true => Fault::ConnectTimeout,
            false => Fault::RequestTimeout,
        };
    }
    if let Some(io) = io {
        let failure = OsError {
            detail: None,
            ..OsError::from(io)
        };
        if failure.code.is_some() || failure.kind != crate::op::IoKind::Other {
            return Fault::Os(failure);
        }
    }
    if error.is_connect() {
        Fault::Connect
    } else if error.is_decode() {
        Fault::Decode
    } else if error.is_body() {
        Fault::Body
    } else {
        Fault::Request
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events() -> Events<FetchEvent> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        Events::new(sender)
    }

    /// A file upload is read from the file itself by each hop that sends it, as far
    /// as the length it had when prepared, and a read that fails while the request
    /// streams keeps its I/O cause.
    #[tokio::test]
    async fn file_uploads_stream_the_file_as_each_hop_finds_it() {
        let root = tempfile::tempdir().unwrap();
        let directory = TargetPath::new(root.path().to_str().unwrap());
        assert!(matches!(
            FileUpload::open(directory, &events()).await,
            Err(Fault::UploadNotRegular)
        ));
        let source = root.path().join("source");
        let original = (0..3 * UPLOAD_CHUNK).map(|i| i as u8).collect::<Vec<_>>();
        tokio::fs::write(&source, &original).await.unwrap();
        let path = TargetPath::new(source.to_str().unwrap());
        let upload = FileUpload::open(path, &events()).await.unwrap();
        assert_eq!(upload.length, original.len() as u64);
        let sent =
            async || (upload.replay().try_collect::<Vec<_>>().await).map(|sent| sent.concat());
        assert_eq!(sent().await.unwrap(), original);
        // The next hop sends what the file holds by then.
        let changed = vec![7; original.len() + 1];
        tokio::fs::write(&source, &changed).await.unwrap();
        assert_eq!(sent().await.unwrap(), changed[..original.len()]);

        tokio::fs::remove_file(&source).await.unwrap();
        // A listener that never answers: the body fails before any reply could.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let upload = Upload::File(upload);
        let request = apply_body(client.put(url), Some(&upload), &HeaderMap::new());
        let failure = transport_error(request.send().await.unwrap_err());
        assert!(matches!(
            failure,
            Fault::Os(OsError {
                kind: crate::op::IoKind::NotFound,
                ..
            })
        ));
    }
}
