//! The code that runs wherever Skyhook's tools execute: the operations tools run, the
//! protocol a host drives them over, and the SSH and WinRM origin that reaches further hops.

pub mod diagnostic;
pub mod fs;
pub mod job;
pub mod local;
pub mod op;
pub mod origin;
pub mod process;
pub mod protocol;
pub mod secret;
pub mod target;
pub mod tls;

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

    /// A FIFO at `path`.
    pub(crate) fn fifo(path: &std::path::Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a NUL-terminated string that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }
}
