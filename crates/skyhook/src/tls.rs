//! The TLS every HTTP client of Skyhook's uses: the bundled Mozilla root list, so the
//! same servers are trusted on any machine, plus this machine's own store where it has
//! one.

use std::sync::{Arc, OnceLock};

use rustls::{ClientConfig, RootCertStore, pki_types::CertificateDer};

/// A client builder trusting the Mozilla roots and this machine's store.
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    static TRUSTED: OnceLock<RootCertStore> = OnceLock::new();
    let trusted = TRUSTED.get_or_init(|| roots(rustls_native_certs::load_native_certs().certs));
    reqwest::Client::builder().use_preconfigured_tls(client_config(trusted.clone()))
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
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the provider supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

#[cfg(test)]
mod tests {
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
}
