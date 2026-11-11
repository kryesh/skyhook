//! TLS for WinRM: the configured trust, the fingerprint of a certificate it refused,
//! and the channel binding that ties authentication to the TLS connection.

use std::{
    fmt,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex, PoisonError},
};

use picky_asn1::{tag::Tag, wrapper::ExplicitContextTag0};
use picky_asn1_der::Asn1RawDer;
use picky_asn1_x509::RawAlgorithmIdentifier;
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject as _},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use skyhook_base::{
    decode::{self, Context, Decode, Node},
    enums::{Kinds, NamedEnum},
    hex::Sha256,
};

use crate::op::TargetPath;
use thiserror::Error;

/// The most of a CA file read.
const MAX_CA_BYTES: u64 = 1024 * 1024;

/// The listener and what is trusted of its certificate: HTTPS, by default on port 5986, or plain HTTP on 5985
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Kinds)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[kinds(name = TlsKind)]
pub enum Tls {
    /// HTTPS trusting the Mozilla roots and the store of the machine connecting
    System,
    /// HTTPS trusting the Mozilla roots and the PEM certificates in this file on the machine connecting; ~/ is the home directory
    Ca(crate::op::TargetPath),
    /// HTTPS trusting exactly the certificate with this SHA-256 fingerprint, whoever issued it
    Fingerprint(Fingerprint),
    /// HTTPS accepting any certificate, whoever issued it and whatever it names
    Insecure,
    /// Plain HTTP
    Off,
}

/// Why CA certificates could not be used.
#[derive(Clone, Debug, Error, Deserialize, Serialize)]
pub enum CaError {
    #[error("{}: {error}", path.display())]
    Read { path: PathBuf, error: String },
    #[error("start with ~/, but there is no home directory")]
    NoHome,
    #[error("are not valid PEM")]
    Pem,
    #[error("hold no certificate")]
    Empty,
}

/// A certificate's SHA-256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Fingerprint(Sha256);

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl Fingerprint {
    pub fn of(certificate: &[u8]) -> Self {
        Self(Sha256::of(certificate))
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Sha256::from_bytes(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

impl TryFrom<String> for Fingerprint {
    type Error = FingerprintError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<Fingerprint> for String {
    fn from(fingerprint: Fingerprint) -> Self {
        fingerprint.to_string()
    }
}

impl JsonSchema for Fingerprint {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Fingerprint".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A certificate's SHA-256 digest: 64 hexadecimal digits, optionally in colon-separated pairs",
            "type": "string",
            "pattern": "^([0-9A-Fa-f]{2}:?){31}[0-9A-Fa-f]{2}$",
        })
    }
}

/// Why text is not a fingerprint.
#[derive(Clone, Copy, Debug, Error)]
#[error("a SHA-256 fingerprint is 64 hexadecimal digits, optionally separated by colons")]
pub struct FingerprintError;

impl FromStr for Fingerprint {
    type Err = FingerprintError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits: String = text.chars().filter(|&c| c != ':').collect();
        Sha256::from_hex(&digits).map(Self).ok_or(FingerprintError)
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The client configuration for one listener, and the fingerprint of the last
/// certificate it refused.
pub(super) struct Trust {
    pub config: ClientConfig,
    verifier: Arc<Verifier>,
}

impl Trust {
    /// The trust `tls` describes, or none for plain HTTP.
    pub(super) async fn new(tls: Tls) -> Result<Option<Self>, CaError> {
        let base = match tls {
            Tls::Off => return Ok(None),
            Tls::System => Base::Roots(roots(crate::tls::trusted().clone())),
            Tls::Ca(path) => Base::Roots(roots(crate::tls::roots(certificates(&path).await?))),
            Tls::Fingerprint(fingerprint) => Base::Pinned(fingerprint),
            Tls::Insecure => Base::Any,
        };
        let verifier = Arc::new(Verifier {
            base,
            provider: crate::tls::provider(),
            refused: Mutex::default(),
        });
        let mut config = crate::tls::config_builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth();
        // HTTP.sys refuses NTLM and Kerberos over HTTP/2.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Some(Self { config, verifier }))
    }

    /// The fingerprint of the certificate a failed handshake refused since last asked.
    pub(super) fn take_refused(&self) -> Option<Fingerprint> {
        (self.verifier.refused.lock())
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

fn roots(roots: RootCertStore) -> Arc<dyn ServerCertVerifier> {
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), crate::tls::provider())
        .build()
        .expect("the Mozilla roots are never empty")
}

async fn certificates(
    path: &crate::op::TargetPath,
) -> Result<Vec<CertificateDer<'static>>, CaError> {
    let path = crate::secret::expand_home(path).ok_or(CaError::NoHome)?;
    let pem = match crate::fs::read_regular(&path, MAX_CA_BYTES).await {
        Ok(pem) => pem,
        Err(error) => {
            let error = error.to_string();
            return Err(CaError::Read { path, error });
        }
    };
    let certificates = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CaError::Pem)?;
    match certificates.is_empty() {
        true => Err(CaError::Empty),
        false => Ok(certificates),
    }
}

enum Base {
    Roots(Arc<dyn ServerCertVerifier>),
    Pinned(Fingerprint),
    Any,
}

struct Verifier {
    base: Base,
    provider: Arc<CryptoProvider>,
    refused: Mutex<Option<Fingerprint>>,
}

impl fmt::Debug for Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Verifier")
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = Fingerprint::of(end_entity);
        let verified = match &self.base {
            Base::Any => Ok(ServerCertVerified::assertion()),
            Base::Pinned(pinned) if *pinned == fingerprint => Ok(ServerCertVerified::assertion()),
            Base::Pinned(_) => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
            Base::Roots(roots) => {
                roots.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
            }
        };
        if verified.is_err() {
            *self.refused.lock().unwrap_or_else(PoisonError::into_inner) = Some(fingerprint);
        }
        verified
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        verify_tls12_signature(message, certificate, signature, algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        verify_tls13_signature(message, certificate, signature, algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        (self.provider.signature_verification_algorithms).supported_schemes()
    }
}

/// Why a certificate has no `tls-server-end-point` channel binding.
#[derive(Clone, Debug, Error, Deserialize, Serialize)]
pub enum BindingError {
    #[error("is not an X.509 certificate")]
    Malformed,
    #[error("is signed with algorithm {0}, which has no supported channel binding")]
    Unsupported(String),
}

/// The `tls-server-end-point` channel binding of RFC 5929 as a `SEC_CHANNEL_BINDINGS`
/// structure: the certificate's hash, by its signature's hash function.
pub(super) fn channel_binding(certificate: &[u8]) -> Result<Vec<u8>, BindingError> {
    let signed: Signed = parse(certificate)?;
    let hash = Hash::of_signature(&signed.algorithm)?.digest(certificate);
    let data = [b"tls-server-end-point:".as_slice(), &hash].concat();
    // No initiator or acceptor addresses; the application data follows the header.
    const HEADER: u32 = 32;
    let length = u32::try_from(data.len()).expect("a digest is short");
    let header = [0, 0, 0, 0, 0, 0, length, HEADER];
    let mut binding: Vec<u8> = header
        .iter()
        .flat_map(|field| field.to_le_bytes())
        .collect();
    binding.extend_from_slice(&data);
    Ok(binding)
}

fn parse<'a, T: Deserialize<'a>>(der: &'a [u8]) -> Result<T, BindingError> {
    picky_asn1_der::from_bytes(der).map_err(|_| BindingError::Malformed)
}

/// A certificate's signed content, left unread, and its signature's algorithm.
#[derive(Deserialize)]
struct Signed {
    _content: Asn1RawDer,
    algorithm: RawAlgorithmIdentifier,
}

/// RSASSA-PSS parameters (RFC 4055), of which only the hash, first, matters; every
/// field is optional and context-tagged.
#[derive(Deserialize)]
struct PssParameters {
    #[serde(default)]
    first: Option<Asn1RawDer>,
}

const ECDSA_WITH_SHA1: &str = "1.2.840.10045.4.1";
const ECDSA_WITH_SHA224: &str = "1.2.840.10045.4.3.1";

#[derive(Clone, Copy)]
enum Hash {
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    /// The hash a signature algorithm with one hash function binds with: its own, with
    /// SHA-256 standing in for MD5 and SHA-1.
    fn of_signature(algorithm: &RawAlgorithmIdentifier) -> Result<Self, BindingError> {
        use picky_asn1_x509::oids::*;
        let oid: String = algorithm.algorithm().into();
        Ok(match oid.as_str() {
            MD5_WITH_RSA_ENCRYPTHION
            | SHA1_WITH_RSA_ENCRYPTION
            | ECDSA_WITH_SHA1
            | DSA_WITH_SHA1 => Self::Sha256,
            SHA224_WITH_RSA_ENCRYPTION | ECDSA_WITH_SHA224 | DSA_WITH_SHA224 => Self::Sha224,
            SHA256_WITH_RSA_ENCRYPTION | ECDSA_WITH_SHA256 | DSA_WITH_SHA256 => Self::Sha256,
            SHA384_WITH_RSA_ENCRYPTION | ECDSA_WITH_SHA384 | DSA_WITH_SHA384 => Self::Sha384,
            SHA512_WITH_RSA_ENCRYPTION | ECDSA_WITH_SHA512 | DSA_WITH_SHA512 => Self::Sha512,
            RSASSA_PSS => Self::of_pss(algorithm.parameters().ok_or(BindingError::Malformed)?)?,
            _ => return Err(BindingError::Unsupported(oid)),
        })
    }

    /// The hash RSASSA-PSS parameters name: SHA-1 when they name none.
    fn of_pss(parameters: &Asn1RawDer) -> Result<Self, BindingError> {
        use picky_asn1_x509::oids::*;
        let parameters: PssParameters = parse(&parameters.0)?;
        let hash = match parameters.first {
            Some(field)
                if field.0.first() == Some(&Tag::context_specific_constructed(0).inner()) =>
            {
                let tagged: ExplicitContextTag0<RawAlgorithmIdentifier> = parse(&field.0)?;
                tagged.0.algorithm().into()
            }
            _ => SHA1.to_owned(),
        };
        match hash.as_str() {
            SHA1 | SHA256 => Ok(Self::Sha256),
            SHA224 => Ok(Self::Sha224),
            SHA384 => Ok(Self::Sha384),
            SHA512 => Ok(Self::Sha512),
            _ => Err(BindingError::Unsupported(format!(
                "{RSASSA_PSS} with {hash}"
            ))),
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha224 => sha2::Sha224::digest(data).to_vec(),
            Self::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(data).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }
}

skyhook_base::decode_from_str!(Fingerprint);

/// A kind without a value by its name, `ca` and `fingerprint` as a mapping of the name to it.
impl Decode for Tls {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        let Some(name) = node.as_str() else {
            let valued = [TlsKind::Ca.as_str(), TlsKind::Fingerprint.as_str()];
            return decode::variant(node, cx, &valued, |key, value, cx| {
                match TlsKind::parse(key) {
                    Some(TlsKind::Ca) => TargetPath::decode(value, cx).map(Self::Ca),
                    _ => Fingerprint::decode(value, cx).map(Self::Fingerprint),
                }
            });
        };
        match TlsKind::parse(name) {
            Some(TlsKind::System) => Some(Self::System),
            Some(TlsKind::Insecure) => Some(Self::Insecure),
            Some(TlsKind::Off) => Some(Self::Off),
            _ => {
                let kinds = decode::one_of(TlsKind::NAMES);
                cx.error(
                    node,
                    format_args!("unknown variant `{name}`, expected one of {kinds}"),
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use picky_asn1_x509::{AlgorithmIdentifier, HashAlgorithm, RsassaPssParams};

    use super::*;

    fn certificate(algorithm: &'static rcgen::SignatureAlgorithm) -> rcgen::Certificate {
        let key = rcgen::KeyPair::generate_for(algorithm).unwrap();
        let params = rcgen::CertificateParams::new(vec!["winrm.test".into()]).unwrap();
        params.self_signed(&key).unwrap()
    }

    /// A certificate whose outer signature names `algorithm`.
    fn signed_with(algorithm: AlgorithmIdentifier) -> Vec<u8> {
        let der = certificate(&rcgen::PKCS_ECDSA_P256_SHA256).der().to_vec();
        let mut parsed: picky_asn1_x509::Certificate = picky_asn1_der::from_bytes(&der).unwrap();
        parsed.signature_algorithm = algorithm;
        picky_asn1_der::to_vec(&parsed).unwrap()
    }

    /// The binding hashes with the certificate's signature hash, SHA-256 for SHA-1 and
    /// for the PSS default, after a header naming only the application data; a
    /// signature without one supported hash has none.
    #[test]
    fn channel_bindings_hash_with_the_signatures_function() {
        let pss = |hash| AlgorithmIdentifier::new_rsassa_pss(RsassaPssParams::new(hash));
        // sha1WithRSAEncryption's identifier turned into RSASSA-PSS with empty parameters,
        // every field at its default, keeps its length.
        let sha1 = AlgorithmIdentifier::new_sha1_with_rsa_encryption();
        let identifier = picky_asn1_der::to_vec(&sha1).unwrap();
        let mut pss_default = signed_with(sha1);
        let end = (pss_default.windows(identifier.len()))
            .rposition(|window| window == identifier)
            .unwrap()
            + identifier.len();
        pss_default[end - 3..end].copy_from_slice(&[10, 0x30, 0]);
        for (der, length) in [
            (
                certificate(&rcgen::PKCS_ECDSA_P256_SHA256).der().to_vec(),
                32,
            ),
            (
                certificate(&rcgen::PKCS_ECDSA_P384_SHA384).der().to_vec(),
                48,
            ),
            (
                signed_with(AlgorithmIdentifier::new_sha1_with_rsa_encryption()),
                32,
            ),
            (signed_with(pss(HashAlgorithm::SHA384)), 48),
            (signed_with(pss(HashAlgorithm::SHA512)), 64),
            (pss_default, 32),
        ] {
            let binding = channel_binding(&der).unwrap();
            let (header, application) = binding.split_at(32);
            let (prefix, hash) = application.split_at(b"tls-server-end-point:".len());
            assert_eq!(
                (prefix, hash.len()),
                (&b"tls-server-end-point:"[..], length)
            );
            let length = u32::try_from(application.len()).unwrap();
            assert_eq!(
                header[24..],
                [length.to_le_bytes(), 32_u32.to_le_bytes()].concat()
            );
        }
        assert!(matches!(
            channel_binding(certificate(&rcgen::PKCS_ED25519).der()),
            Err(BindingError::Unsupported(oid)) if oid == "1.3.101.112"
        ));
        assert!(matches!(
            channel_binding(&signed_with(pss(HashAlgorithm::SHA3_256))),
            Err(BindingError::Unsupported(_))
        ));
        assert!(matches!(
            channel_binding(b"not a certificate"),
            Err(BindingError::Malformed)
        ));
    }

    /// A pin trusts exactly its certificate and `insecure` any; roots that do not vouch
    /// for one refuse it, and either refusal leaves the certificate's fingerprint;
    /// fingerprints parse with or without colons.
    #[tokio::test]
    async fn refusals_carry_the_certificates_fingerprint() {
        let der = certificate(&rcgen::PKCS_ECDSA_P256_SHA256).der().clone();
        let name = ServerName::try_from("winrm.test").unwrap();
        let verify = |trust: &Trust| {
            (trust.verifier)
                .verify_server_cert(&der, &[], &name, &[], UnixTime::now())
                .is_ok()
        };
        let fingerprint = Fingerprint::of(&der);
        let colons = (fingerprint.to_string().as_bytes().chunks(2))
            .map(|pair| std::str::from_utf8(pair).unwrap())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(colons.parse::<Fingerprint>().unwrap(), fingerprint);
        assert!("00".parse::<Fingerprint>().is_err());

        for tls in [Tls::Fingerprint(fingerprint), Tls::Insecure] {
            let trust = Trust::new(tls).await.unwrap().unwrap();
            assert!(verify(&trust) && trust.take_refused().is_none());
        }
        let other = tempfile::NamedTempFile::new().unwrap();
        let pem = certificate(&rcgen::PKCS_ECDSA_P256_SHA256).pem();
        std::fs::write(other.path(), pem).unwrap();
        for tls in [
            Tls::Fingerprint(Fingerprint::from_bytes([0; 32])),
            Tls::System,
            Tls::Ca(other.path().to_str().unwrap().into()),
        ] {
            let trust = Trust::new(tls).await.unwrap().unwrap();
            assert!(!verify(&trust));
            assert_eq!(trust.take_refused(), Some(fingerprint));
        }
        let empty = tempfile::NamedTempFile::new().unwrap();
        assert!(matches!(
            Trust::new(Tls::Ca(empty.path().to_str().unwrap().into())).await,
            Err(CaError::Empty)
        ));
    }
}
