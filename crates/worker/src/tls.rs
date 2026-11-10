//! The TLS every HTTP client of Skyhook's uses: the bundled Mozilla root list, so the
//! same servers are trusted on any machine, plus this machine's own store where it has
//! one.

use std::sync::{Arc, OnceLock};

use rustls::{
    ClientConfig, ConfigBuilder, RootCertStore, WantsVerifier, crypto::CryptoProvider,
    pki_types::CertificateDer,
};

/// A client builder trusting the Mozilla roots and this machine's store.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().use_preconfigured_tls(client_config(trusted().clone()))
}

/// The Mozilla roots and this machine's store, loaded once.
pub(crate) fn trusted() -> &'static RootCertStore {
    static TRUSTED: OnceLock<RootCertStore> = OnceLock::new();
    TRUSTED.get_or_init(|| roots(rustls_native_certs::load_native_certs().certs))
}

/// The Mozilla roots and those of `extra` that parse; the others are left out.
pub(crate) fn roots(extra: impl IntoIterator<Item = CertificateDer<'static>>) -> RootCertStore {
    let mut roots: RootCertStore = webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
    roots.add_parsable_certificates(extra);
    roots
}

/// A client configuration trusting `roots`, offering HTTP/2 and HTTP/1.1 as reqwest's
/// own does.
pub(crate) fn client_config(roots: RootCertStore) -> ClientConfig {
    let mut config = config_builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

/// The cryptography every client configuration uses.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// A client configuration with its protocol versions chosen, still to be told what it
/// trusts.
pub(crate) fn config_builder() -> ConfigBuilder<ClientConfig, WantsVerifier> {
    ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("the provider supports the default protocol versions")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Without anything else, the Mozilla roots are trusted; what else parses is
    /// trusted beside them, and what does not is left out.
    #[test]
    fn roots_are_the_mozilla_list_and_what_else_parses() {
        let mozilla = roots([]);
        let named = |roots: &RootCertStore, name: &[u8]| {
            (roots.roots.iter())
                .any(|anchor| anchor.subject.windows(name.len()).any(|part| part == name))
        };
        assert!(named(&mozilla, b"ISRG Root X1"));
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let unparsable = CertificateDer::from(b"not a certificate".to_vec());
        let extended = roots([certificate.cert.der().clone(), unparsable]);
        assert_eq!(extended.len(), mozilla.len() + 1);
    }

    /// Serves `connections` TLS accepts; returns (rejected handshakes, requests).
    /// Aborted on drop so a failed assertion leaves no detached server task.
    pub(crate) struct SelfSignedServer(pub(crate) tokio::task::JoinHandle<(usize, Vec<String>)>);

    impl Drop for SelfSignedServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    /// The server's URL, its task, and the certificate it serves.
    pub(crate) async fn self_signed_server(
        connections: usize,
    ) -> (String, SelfSignedServer, CertificateDer<'static>) {
        use crate::tests::{bounded, read_request};
        use tokio::io::AsyncWriteExt as _;
        const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 6\r\nConnection: close\r\n\r\ntls-ok";
        // A fresh, currently valid certificate with matching IP/DNS SANs isolates
        // the failure to its untrusted issuer, not expiration or hostname mismatch.
        let certificate =
            rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
                .unwrap();
        let key =
            rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der());
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.cert.der().clone()], key.into())
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
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
        let served = certificate.cert.der().clone();
        (url, SelfSignedServer(task), served)
    }

    /// The shared TLS trusts a test CA only when it is among the roots it is given.
    #[tokio::test]
    async fn shared_tls_trusts_a_test_ca_only_when_given_it() {
        let (url, mut server, certificate) = self_signed_server(2).await;
        for (extra, trusted) in [(None, false), (Some(certificate), true)] {
            let config = client_config(roots(extra));
            let client = reqwest::Client::builder()
                .no_proxy()
                .use_preconfigured_tls(config);
            let sent = crate::tests::bounded(client.build().unwrap().get(&url).send()).await;
            assert_eq!(sent.is_ok(), trusted, "{sent:?}");
        }
        let (rejected, requests) = crate::tests::bounded(&mut server.0).await.unwrap();
        assert_eq!((rejected, requests.len()), (1, 1));
    }
}
