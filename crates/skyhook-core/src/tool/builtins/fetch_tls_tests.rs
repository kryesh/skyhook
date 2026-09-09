//! Real TLS coverage through the registered fetch tool, without external services.
use std::{sync::Arc, time::Duration};

use rcgen::generate_simple_self_signed;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{ServerConfig, crypto::ring, pki_types::PrivatePkcs8KeyDer},
};

use crate::{
    test_support::TestRuntime,
    tool::{ToolRegistryBuilder, executor::ToolExecutor},
};

const LIMIT: Duration = Duration::from_secs(15);
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 6\r\nConnection: close\r\n\r\ntls-ok";

#[derive(Debug, Default)]
struct ServerStats {
    rejected_handshakes: usize,
    requests: Vec<String>,
}

struct SelfSignedServer {
    url: String,
    task: JoinHandle<ServerStats>,
}

impl SelfSignedServer {
    async fn start(connections: usize) -> Self {
        // A fresh, currently valid certificate with matching IP/DNS SANs isolates
        // the failure to its untrusted issuer, not expiration or hostname mismatch.
        let certificate =
            generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
        let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("https://{}/tls", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut stats = ServerStats::default();
            for _ in 0..connections {
                let (socket, _) = timeout(LIMIT, listener.accept())
                    .await
                    .expect("fetch did not connect to the local TLS server")
                    .unwrap();
                let handshake = timeout(LIMIT, acceptor.accept(socket))
                    .await
                    .expect("TLS handshake did not finish");
                let mut stream = match handshake {
                    Ok(stream) => stream,
                    Err(_) => {
                        stats.rejected_handshakes += 1;
                        continue;
                    }
                };
                let request = timeout(LIMIT, async {
                    let mut request = Vec::new();
                    loop {
                        let mut buffer = [0_u8; 1024];
                        let read = stream.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0, "TLS client closed before sending HTTP headers");
                        request.extend_from_slice(&buffer[..read]);
                        assert!(request.len() <= 8192, "unexpectedly large test request");
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    stream.write_all(RESPONSE).await.unwrap();
                    stream.flush().await.unwrap();
                    let _ = stream.shutdown().await;
                    String::from_utf8(request).unwrap()
                })
                .await
                .expect("local HTTPS request did not finish");
                stats.requests.push(request);
            }
            stats
        });
        Self { url, task }
    }

    async fn finish(&mut self) -> ServerStats {
        timeout(LIMIT, &mut self.task)
            .await
            .expect("local TLS server did not finish")
            .expect("local TLS server failed")
    }
}

impl Drop for SelfSignedServer {
    fn drop(&mut self) {
        // Also stop the listener on a failed assertion; no detached server tasks.
        self.task.abort();
    }
}

async fn fetch(
    runtime: &TestRuntime,
    executor: &ToolExecutor,
    url: &str,
    insecure: Option<bool>,
) -> Value {
    let mut arguments = json!({"url": url, "timeout": 5, "connect_timeout": 5});
    if let Some(insecure) = insecure {
        arguments["insecure"] = json!(insecure);
    }
    timeout(
        LIMIT,
        executor.execute_model(runtime.agent.clone(), "fetch", arguments, None),
    )
    .await
    .expect("registered fetch did not finish")
    .expect("registered fetch could not start")
    .output
    .value
}

#[tokio::test]
async fn insecure_is_opt_in_and_does_not_leak_between_registered_fetch_calls() {
    let runtime = TestRuntime::new().await;
    let mut builder = ToolRegistryBuilder::default();
    super::register(&mut builder).unwrap();
    let executor = runtime.executor(builder);
    let surface = executor.surface();
    let schema = &surface.get("fetch").unwrap().input_schema;
    assert_eq!(schema["properties"]["insecure"]["type"], "boolean");
    assert_eq!(schema["properties"]["insecure"]["default"], false);
    assert!(
        !schema["required"]
            .as_array()
            .is_some_and(|required| required.iter().any(|field| field == "insecure")),
        "insecure must be optional in the advertised tool schema"
    );

    let mut server = SelfSignedServer::start(5).await;
    // Reuse one executor and one HTTPS origin throughout. Both strict forms are
    // tested before AND after the opt-out, catching a shared permissive client.
    for insecure in [None, Some(false)] {
        let output = fetch(&runtime, &executor, &server.url, insecure).await;
        assert_eq!(output["state"], "failed", "{output:#}");
    }

    let output = fetch(&runtime, &executor, &server.url, Some(true)).await;
    assert_eq!(output["state"], "completed", "{output:#}");
    assert_eq!(output["result"]["status"], 200, "{output:#}");
    assert_eq!(output["result"]["ok"], true, "{output:#}");
    assert_eq!(output["result"]["body"]["kind"], "text", "{output:#}");
    assert_eq!(output["result"]["body"]["text"], "tls-ok", "{output:#}");

    for insecure in [Some(false), None] {
        let output = fetch(&runtime, &executor, &server.url, insecure).await;
        assert_eq!(output["state"], "failed", "{output:#}");
    }

    let stats = server.finish().await;
    assert_eq!(stats.rejected_handshakes, 4, "{stats:#?}");
    assert_eq!(stats.requests.len(), 1, "{stats:#?}");
    assert!(stats.requests[0].starts_with("GET /tls HTTP/1.1\r\n"));
}
