//! Per-call HTTP client configuration, system DNS, and opt-in insecure TLS.

use reqwest::Client;

use super::diagnostics::{DnsFailure, FetchDiagnostic, FetchError, FetchPhase};
use super::validation::ClientSettings;

/// Same system lookup as reqwest's default resolver, with a typed failure marker.
/// There is no additional lookup or preflight connection.
#[derive(Debug)]
struct FetchResolver;
impl reqwest::dns::Resolve for FetchResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map_err(|error| {
                    Box::new(DnsFailure::new(error)) as Box<dyn std::error::Error + Send + Sync>
                })?
                .collect::<Vec<_>>();
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

pub(super) fn client(settings: &ClientSettings) -> Result<Client, FetchError> {
    let mut builder = Client::builder()
        .user_agent(concat!("Skyhook/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .dns_resolver(std::sync::Arc::new(FetchResolver))
        .connect_timeout(settings.connect_timeout)
        .timeout(settings.timeout)
        .danger_accept_invalid_certs(settings.insecure);
    // Proxy admission belongs here (before any send), preserving the existing
    // client-preparation diagnostic and permission-order contract. Proxy schemes
    // and URL credentials intentionally differ from HttpRequestUrl admission.
    if let Some(proxy) = &settings.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|error| {
            FetchDiagnostic::from_reqwest(&error, FetchPhase::ClientPreparation)
        })?);
    }
    builder.build().map_err(|error| {
        FetchDiagnostic::from_reqwest(&error, FetchPhase::ClientPreparation).into()
    })
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use super::super::tests::{executor, fetch, read_request, response, server};
    use rcgen::generate_simple_self_signed;
    use serde_json::json;
    use tokio::{io::AsyncWriteExt, net::TcpListener, task::JoinHandle, time::timeout};
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{ServerConfig, crypto::ring, pki_types::PrivatePkcs8KeyDer},
    };

    use crate::tests::TestRuntime;

    const LIMIT: Duration = Duration::from_secs(15);
    const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 6\r\nConnection: close\r\n\r\ntls-ok";

    /// Serves `connections` TLS accepts; returns (rejected handshakes, requests).
    /// Aborted on drop so a failed assertion leaves no detached server task.
    struct SelfSignedServer(JoinHandle<(usize, Vec<String>)>);

    impl Drop for SelfSignedServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn self_signed_server(connections: usize) -> (String, SelfSignedServer) {
        // A fresh, currently valid certificate with matching IP/DNS SANs isolates
        // the failure to its untrusted issuer, not expiration or hostname mismatch.
        let certificate =
            generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
        let key = PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into();
        let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.cert.der().clone()], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("https://{}/tls", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut rejected, mut requests) = (0, Vec::new());
            for _ in 0..connections {
                let accepted = timeout(LIMIT, listener.accept()).await;
                let (socket, _) = accepted
                    .expect("fetch did not connect to the local TLS server")
                    .unwrap();
                let handshake = timeout(LIMIT, acceptor.accept(socket)).await;
                let Ok(mut stream) = handshake.expect("TLS handshake did not finish") else {
                    rejected += 1;
                    continue;
                };
                let request = timeout(LIMIT, async {
                    let request = read_request(&mut stream).await;
                    stream.write_all(RESPONSE).await.unwrap();
                    stream.flush().await.unwrap();
                    let _ = stream.shutdown().await;
                    request
                });
                requests.push(request.await.expect("local HTTPS request did not finish"));
            }
            (rejected, requests)
        });
        (url, SelfSignedServer(task))
    }

    #[tokio::test]
    async fn insecure_is_opt_in_and_does_not_leak_between_registered_fetch_calls() {
        let runtime = TestRuntime::new().await;
        let executor = executor(&runtime);
        let (url, mut server) = self_signed_server(5).await;
        // Reuse one executor and one HTTPS origin throughout. Both strict forms are
        // tested before AND after the opt-out, catching a shared permissive client.
        for insecure in [None, Some(false), Some(true), Some(false), None] {
            let mut arguments = json!({"url":url, "timeout":5, "connect_timeout":5});
            if let Some(insecure) = insecure {
                arguments["insecure"] = json!(insecure);
            }
            let output = timeout(
                LIMIT,
                executor.run_model(&runtime.agent, "fetch", arguments),
            );
            let output = output
                .await
                .expect("registered fetch did not finish")
                .unwrap()
                .output
                .value;
            if insecure == Some(true) {
                assert_eq!(output["state"], "completed", "{output:#}");
                assert_eq!(output["result"]["body"]["text"], "tls-ok", "{output:#}");
            } else {
                assert_eq!(output["state"], "failed", "{output:#}");
            }
        }
        let finished = timeout(LIMIT, &mut server.0)
            .await
            .expect("local TLS server did not finish");
        let (rejected, requests) = finished.expect("local TLS server failed");
        assert_eq!((rejected, requests.len()), (4, 1), "{requests:#?}");
        assert!(requests[0].starts_with("GET /tls HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn default_user_agent_can_be_overridden() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let (url, task) = server(vec![response("200 OK", "", ""); 3]).await;
        let default = concat!("Skyhook/", env!("CARGO_PKG_VERSION"));
        let cases = [
            (json!({}), default),
            (json!({"User-Agent":"CustomClient/2"}), "CustomClient/2"),
            (json!({}), default),
        ];
        for (headers, _) in &cases {
            fetch(&runtime, &executor, json!({"url":url,"headers":headers}))
                .await
                .unwrap();
        }
        for (request, (_, expected)) in task.await.unwrap().iter().zip(cases) {
            let head = request.split("\r\n\r\n").next().unwrap();
            let values: Vec<_> = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
                .map(|(_, value)| value.trim())
                .collect();
            assert_eq!(values, vec![expected]);
        }
    }
}
