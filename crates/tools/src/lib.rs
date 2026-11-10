//! Skyhook's tools: the target tools as specs, how their arguments are validated
//! and their output captured, how their paths resolve, and the facts their failures
//! carry.

pub mod arguments;
pub mod builtins;
pub mod diagnostic;
pub mod error;
pub mod output;
pub mod path;
pub mod schema;
pub mod source;
pub mod spec;
pub mod target;

#[cfg(test)]
mod tests {
    /// Await `future`, failing the test at the caller if it stalls.
    #[track_caller]
    pub(crate) fn bounded<T>(future: impl Future<Output = T>) -> impl Future<Output = T> {
        let caller = std::panic::Location::caller();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(20), future)
                .await
                .unwrap_or_else(|_| panic!("test synchronization timed out at {caller}"))
        }
    }

    /// A 1x1 PNG followed by `tail`, which decoding ignores: distinct tails make
    /// distinct blobs.
    pub(crate) fn png_bytes(tail: &[u8]) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        [bytes.into_inner(), tail.to_vec()].concat()
    }

    /// [`png_bytes`] as an image.
    pub(crate) async fn png(tail: &[u8]) -> skyhook_media::Image {
        skyhook_media::Image::decode(png_bytes(tail)).await.unwrap()
    }

    /// One HTTP request's head and the body its `Content-Length` declares.
    pub(crate) async fn read_request(socket: &mut (impl tokio::io::AsyncRead + Unpin)) -> String {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0u8; 4096];
            let n = socket.read(&mut buffer).await.unwrap();
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(end) = bytes.array_windows().position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|s| s.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    /// A test's own path as the local target spells it.
    pub(crate) fn text(path: impl AsRef<std::path::Path>) -> skyhook_worker::op::TargetPath {
        path.as_ref().to_str().unwrap().into()
    }
}
