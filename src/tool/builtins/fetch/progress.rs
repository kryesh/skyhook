//! Per-operation context retained even when the outer deadline cancels a request.
use std::error::Error;
use std::time::Instant;

use crate::tool::diagnostic::{
    Cause, Effects, IoKind, Operation, PartialContext, RequestEffects, Subject,
};

use super::LocalError;

pub(super) struct FetchProgress {
    started: Instant,
    context: PartialContext,
    /// Once a request is sent, a failure no longer proves the server did nothing.
    request: RequestEffects,
}

impl FetchProgress {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            context: PartialContext::default(),
            request: RequestEffects::Unknown,
        }
    }

    /// Recorded before work begins, so it survives an await cancelled by the outer deadline.
    pub fn operation(&mut self, context: PartialContext) {
        self.context = context;
    }

    pub fn step(&mut self, operation: Operation, subject: Subject) {
        self.operation(PartialContext::new(operation, subject));
    }

    /// Download staging never touches the destination before its final rename.
    pub fn download_io(&mut self, operation: Operation, path: &std::path::Path) {
        self.operation(
            PartialContext::new(operation, Subject::path(path)).effects(Effects::Unchanged),
        );
    }

    pub fn request_started(&mut self, request: String) {
        self.request = RequestEffects::MayHaveExecuted;
        self.step(Operation::Send, Subject::Label(request));
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    pub fn timeout(&self, limit_secs: u64) -> LocalError {
        self.failure(LocalError::failed(format!(
            "the {limit_secs} s timeout elapsed"
        )))
    }

    /// Fill the facts the failure site left unset from the current operation.
    pub fn failure(&self, error: LocalError) -> LocalError {
        error.or(self.context.clone().request(self.request))
    }
}

/// HTTP, TLS, resolver and I/O error displays can contain URLs, credentials,
/// response data or local paths, so none is copied: a transport failure carries
/// a typed I/O kind and OS code, or a fixed message.
pub(super) fn transport_error(error: reqwest::Error) -> LocalError {
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
            return LocalError::failed("the TLS handshake failed");
        }
        io = source.downcast_ref::<std::io::Error>().or(io);
    }
    if error.is_timeout() {
        return LocalError::failed(if error.is_connect() {
            "connect_timeout elapsed while connecting"
        } else {
            "the request timed out"
        });
    }
    if let Some(io) = io {
        let kind = IoKind::from(io.kind());
        let code = io.raw_os_error();
        if code.is_some() || kind != IoKind::Other {
            return LocalError::cause(Cause::Io {
                kind,
                code,
                detail: None,
            });
        }
    }
    LocalError::failed(if error.is_connect() {
        "connecting to the server failed"
    } else if error.is_decode() {
        "decoding the response failed"
    } else if error.is_body() {
        "the response body failed"
    } else {
        "the HTTP request failed"
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{executor, fetch};
    use super::*;
    use crate::provider::http::transport::tests::{Plan, Server};
    use crate::tool::diagnostic::FailureSite;
    use serde_json::json;
    use std::time::Duration;

    /// Wait until a download's staging file holds `bytes`: the client has read
    /// the response head and that much of its body.
    async fn staged(directory: &std::path::Path, bytes: u64) {
        let holds = || {
            std::fs::read_dir(directory)
                .unwrap()
                .flatten()
                .any(|entry| {
                    entry.file_name().to_string_lossy().starts_with(".skyhook-")
                        && entry
                            .metadata()
                            .is_ok_and(|metadata| metadata.len() == bytes)
                })
        };
        crate::tests::bounded(async {
            while !holds() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    /// Failures are plain failed jobs: no structured result, the operation in
    /// progress, and a typed transport cause.
    #[tokio::test]
    async fn transport_failures_and_deadlines_name_the_operation_in_progress() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let hour = Duration::from_secs(3600);
        // A dropped listener's port can be rebound by another test's server
        // before the connect; the privileged port is never listened on.
        let url = "http://127.0.0.1:1/private?token=secret#secret";
        let error = fetch(&runtime, &executor, json!({"url":url}))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("secret"));
        let (diagnostic, output) = error.into_parts();
        assert!(output.is_none());
        assert!(matches!(
            diagnostic.cause,
            Cause::Io {
                kind: IoKind::ConnectionRefused,
                code: Some(_),
                ..
            }
        ));
        assert_eq!(
            diagnostic.context.subject,
            Subject::Label("HTTP GET http://127.0.0.1:1".into())
        );

        // The deadline elapses after the head and part of the body arrived.
        let partial = "partial";
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{partial}");
        let server = Server::start(vec![Plan::stalled(head.as_bytes())]).await;
        let arguments = json!({"url":server.root(), "timeout":3600, "save_to":"download"});
        let fetched = fetch(&runtime, &executor, arguments);
        let received = staged(runtime.root.path(), partial.len() as u64);
        let (result, ()) = crate::tests::expire(fetched, received, hour).await;
        let diagnostic = result.unwrap_err().diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Receive);
        assert_eq!(
            diagnostic.context.subject,
            Subject::path(runtime.root.path().join("download"))
        );
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        assert_eq!(diagnostic.context.request, RequestEffects::MayHaveExecuted);
        assert_eq!(
            diagnostic.cause,
            Cause::Message("the 3600 s timeout elapsed".into())
        );
        assert!(!runtime.root.path().join("download").exists());
        assert_eq!(server.finish().await.len(), 1);

        // The deadline elapses once the server holds the request it never answers.
        let mut server = Server::start(vec![Plan::stalled_headers()]).await;
        let arguments = json!({"url":server.root(), "timeout":3600});
        let fetched = fetch(&runtime, &executor, arguments);
        let (result, _) = crate::tests::expire(fetched, server.request(), hour).await;
        let context = result.unwrap_err().diagnostic().context;
        assert_eq!(context.operation, Operation::Send);
        assert_eq!(context.request, RequestEffects::MayHaveExecuted);
        assert!(server.finish().await.is_empty(), "unexpected replay");
    }

    /// Cancelled once the body is being staged, a download names the staging
    /// step, leaves the destination as it was and removes what it staged.
    #[tokio::test]
    async fn cancellation_while_staging_names_the_download_and_leaves_the_destination() {
        let runtime = crate::tests::TestRuntime::new().await;
        let destination = runtime.root.path().join("saved.txt");
        tokio::fs::write(&destination, b"original").await.unwrap();
        let partial = "partial";
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{partial}");
        let server = Server::start(vec![Plan::stalled(head.as_bytes())]).await;
        let (executor, agent) = (executor(&runtime), runtime.agent.clone());
        let arguments = json!({"url":server.root(), "save_to":"saved.txt", "overwrite":true});
        let pending =
            tokio::spawn(async move { executor.run_host(&agent, "fetch", arguments).await });
        staged(runtime.root.path(), partial.len() as u64).await;
        let scope = crate::job::CancelScope::Outcome;
        assert_eq!(runtime.jobs.cancel_all(&runtime.agent, scope).await, 1);
        let result = crate::tests::bounded(pending).await.unwrap();
        let diagnostic = result.unwrap_err().diagnostic();
        assert_eq!(diagnostic.cause, Cause::Cancelled);
        assert_eq!(diagnostic.context.operation, Operation::Receive);
        assert_eq!(diagnostic.context.subject, Subject::path(&destination));
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        assert_eq!(diagnostic.context.request, RequestEffects::MayHaveExecuted);
        assert_eq!(server.finish().await.len(), 1);
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"original");
        let staging = std::fs::read_dir(runtime.root.path()).unwrap().flatten();
        let staging =
            staging.filter(|entry| entry.file_name().to_string_lossy().starts_with(".skyhook-"));
        assert_eq!(staging.count(), 0);
    }

    #[test]
    fn failures_keep_the_context_their_site_chose() {
        let mut progress = FetchProgress::new();
        progress.request_started("HTTP GET https://example.org".into());
        let authorization = PartialContext::new(
            Operation::Authorize,
            Subject::Label("network permission".into()),
        )
        .at(FailureSite::Host)
        .effects(Effects::NotStarted);
        let denied = progress
            .failure(LocalError::denied("policy").context(authorization.clone()))
            .diagnostic();
        // A denied redirect hop does not undo the request already sent.
        assert_eq!(
            denied.context,
            authorization
                .request(RequestEffects::MayHaveExecuted)
                .resolve()
        );
        let cancelled = progress.failure(LocalError::cancelled()).diagnostic();
        assert_eq!(
            cancelled.context,
            PartialContext::new(
                Operation::Send,
                Subject::Label("HTTP GET https://example.org".into())
            )
            .request(RequestEffects::MayHaveExecuted)
            .resolve()
        );
    }
}
