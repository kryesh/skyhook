//! Fetch's HTTP clients, each shared by every call with the same client settings,
//! and opt-in insecure TLS.

use std::sync::{Mutex, MutexGuard, PoisonError};

use reqwest::Client;

use super::LocalError;
use super::validation::ClientIdentity;

/// How many clients of distinct settings are kept before the cache starts over.
const CLIENTS: usize = 8;

static CLIENTS_BUILT: Mutex<Vec<(ClientIdentity, Client)>> = Mutex::new(Vec::new());

fn clients() -> MutexGuard<'static, Vec<(ClientIdentity, Client)>> {
    CLIENTS_BUILT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The client for `identity`, with the connections it keeps open. A miss builds one
/// off the async workers, since that loads the platform's trusted roots; calls that
/// miss together each build one, and the first built is kept.
pub(super) async fn client(identity: &ClientIdentity) -> Result<Client, LocalError> {
    let built = |clients: &[(ClientIdentity, Client)]| {
        let found = clients.iter().find(|(built, _)| built == identity);
        found.map(|(_, client)| client.clone())
    };
    if let Some(client) = built(&clients()) {
        return Ok(client);
    }
    let building = identity.clone();
    let client = crate::blocking(move || build(&building)).await?;
    let mut clients = clients();
    if let Some(kept) = built(&clients) {
        return Ok(kept);
    }
    if clients.len() == CLIENTS {
        clients.clear();
    }
    clients.push((identity.clone(), client.clone()));
    Ok(client)
}

fn build(identity: &ClientIdentity) -> Result<Client, LocalError> {
    let mut builder = Client::builder()
        .user_agent(concat!("Skyhook/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        // The tool's outer deadline is the only total limit: an equal client
        // timeout would race it and could report the timeout without its limit.
        .connect_timeout(identity.connect_timeout)
        .danger_accept_invalid_certs(identity.insecure);
    // Proxy schemes and URL credentials intentionally differ from HttpRequestUrl admission.
    if let Some(proxy) = &identity.proxy {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|_| LocalError::invalid_arguments("invalid proxy URL"))?,
        );
    }
    builder
        .build()
        .map_err(|_| LocalError::failed("the HTTP client could not be built"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::tests::executor;
    use crate::provider::http::transport::tests::read_request;
    use rcgen::generate_simple_self_signed;
    use serde_json::json;
    use tokio::{io::AsyncWriteExt, net::TcpListener, task::JoinHandle};
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{ServerConfig, crypto::ring, pki_types::PrivatePkcs8KeyDer},
    };

    use crate::tests::{TestRuntime, bounded};

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
                let (socket, _) = bounded(listener.accept()).await.unwrap();
                let Ok(mut stream) = bounded(acceptor.accept(socket)).await else {
                    rejected += 1;
                    continue;
                };
                let request = bounded(async {
                    let request = read_request(&mut stream).await;
                    stream.write_all(RESPONSE).await.unwrap();
                    stream.flush().await.unwrap();
                    let _ = stream.shutdown().await;
                    request
                });
                requests.push(request.await);
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
            let mut arguments = json!({"url":url});
            if let Some(insecure) = insecure {
                arguments["insecure"] = json!(insecure);
            }
            let output = bounded(executor.run_model(&runtime.agent, "fetch", arguments))
                .await
                .unwrap()
                .output
                .value;
            if insecure == Some(true) {
                assert_eq!(output.get("state"), None, "{output:#}");
                assert_eq!(output["result"]["body"]["text"], "tls-ok", "{output:#}");
            } else {
                assert_eq!(output["state"], "failed", "{output:#}");
                assert!(
                    output["error"]
                        .as_str()
                        .unwrap()
                        .contains("TLS handshake failed"),
                    "{output:#}"
                );
            }
        }
        let (rejected, requests) = bounded(&mut server.0).await.unwrap();
        assert_eq!((rejected, requests.len()), (4, 1), "{requests:#?}");
        assert!(requests[0].starts_with("GET /tls HTTP/1.1\r\n"));
    }
}
