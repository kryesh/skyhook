//! An NTLM or Kerberos security context: the tokens of its handshake, and once it is
//! established, the sealing of messages.

use sspi::{
    AuthIdentity, AuthIdentityBuffers, BufferType, ClientRequestFlags, CredentialUse, Credentials,
    CredentialsBuffers, DataRepresentation, EncryptionFlags, Kerberos, KerberosConfig, Ntlm,
    SecurityBuffer, SecurityBufferRef, SecurityStatus, Sspi, SspiImpl, generator::GeneratorState,
};
use url::Url;

use super::{error::WinrmError, framing::Sealed, kerberos};

/// What a context authenticates with.
pub(super) enum Package {
    Ntlm(AuthIdentity),
    Kerberos { identity: AuthIdentity, kdc: Url },
}

/// What carries the connection a context authenticates.
pub(super) enum Carrier {
    /// TLS, and its `SEC_CHANNEL_BINDINGS` when the listener showed its certificate.
    Tls(Option<Vec<u8>>),
    /// Plain HTTP, over which the context seals every message.
    Http,
}

/// One connection's security context.
pub(super) struct Context {
    mechanism: Mechanism,
    spn: String,
    flags: ClientRequestFlags,
    binding: Option<Vec<u8>>,
}

enum Mechanism {
    Ntlm(Box<Ntlm>, Option<AuthIdentityBuffers>),
    Kerberos(Box<Kerberos>, Option<CredentialsBuffers>),
}

/// What the context answered a step of its handshake with: a token for the peer.
pub(super) enum Step {
    /// The context waits for the peer's answer to the token.
    Continue(Vec<u8>),
    /// The context is established; the token, if any, is its last.
    Established(Vec<u8>),
}

impl Context {
    /// A context authenticating to service `spn` over `carrier`.
    pub(super) fn new(
        package: &Package,
        spn: String,
        carrier: Carrier,
    ) -> Result<Self, WinrmError> {
        let (sealing, binding) = match carrier {
            Carrier::Tls(binding) => (ClientRequestFlags::empty(), binding),
            Carrier::Http => (
                ClientRequestFlags::CONFIDENTIALITY | ClientRequestFlags::INTEGRITY,
                None,
            ),
        };
        let (mechanism, flags) = match package {
            Package::Ntlm(identity) => {
                let mut ntlm = Box::new(Ntlm::new());
                let acquired = (ntlm.acquire_credentials_handle())
                    .with_credential_use(CredentialUse::Outbound)
                    .with_auth_data(identity)
                    .execute(&mut *ntlm)?;
                (Mechanism::Ntlm(ntlm, acquired.credentials_handle), sealing)
            }
            Package::Kerberos { identity, kdc } => {
                let config = KerberosConfig {
                    kdc_resolution: sspi::KdcResolution::KdcUrl(Some(kdc.clone())),
                    client_computer_name: kerberos::computer_name(),
                };
                let mut client = Box::new(Kerberos::new_client_from_config(config)?);
                let credentials = Credentials::AuthIdentity(identity.clone());
                let acquired = (client.acquire_credentials_handle())
                    .with_credential_use(CredentialUse::Outbound)
                    .with_auth_data(&credentials)
                    .execute(&mut *client)?;
                let flags = sealing | ClientRequestFlags::MUTUAL_AUTH;
                (
                    Mechanism::Kerberos(client, acquired.credentials_handle),
                    flags,
                )
            }
        };
        Ok(Self {
            mechanism,
            spn,
            flags: flags | ClientRequestFlags::ALLOCATE_MEMORY,
            binding,
        })
    }

    /// The next token of the handshake, answering the peer's `input` if it sent one.
    pub(super) async fn step(&mut self, input: Option<Vec<u8>>) -> Result<Step, WinrmError> {
        let mut buffers = vec![SecurityBuffer::new(
            input.unwrap_or_default(),
            BufferType::Token,
        )];
        if let Some(binding) = &self.binding {
            buffers.push(SecurityBuffer::new(
                binding.clone(),
                BufferType::ChannelBindings,
            ));
        }
        let (flags, spn) = (self.flags, self.spn.as_str());
        let (status, token) = match &mut self.mechanism {
            Mechanism::Ntlm(ntlm, credentials) => {
                initialize(&mut **ntlm, credentials, flags, spn, &mut buffers).await?
            }
            Mechanism::Kerberos(client, credentials) => {
                initialize(&mut **client, credentials, flags, spn, &mut buffers).await?
            }
        };
        Ok(match status {
            SecurityStatus::Ok => Step::Established(token),
            _ => Step::Continue(token),
        })
    }

    /// `message` sealed and framed for an encrypted request.
    pub(super) fn seal(&mut self, message: &[u8]) -> Result<Vec<u8>, WinrmError> {
        match &mut self.mechanism {
            Mechanism::Ntlm(ntlm, _) => seal(&mut **ntlm, message),
            Mechanism::Kerberos(client, _) => seal(&mut **client, message),
        }
    }

    /// The message an encrypted reply holds, checked against its signature and length.
    pub(super) fn unseal(&mut self, body: &[u8]) -> Result<Vec<u8>, WinrmError> {
        let framed = Sealed::unframe(body)?;
        let message = match &mut self.mechanism {
            Mechanism::Ntlm(ntlm, _) => unseal(&mut **ntlm, &framed),
            Mechanism::Kerberos(client, _) => unseal(&mut **client, &framed),
        }?;
        match message.len() == framed.length {
            true => Ok(message),
            false => Err(super::error::DecodeError::Length.into()),
        }
    }
}

/// One `InitializeSecurityContext` call, its KDC exchanges made here. A failed
/// exchange is reported as itself rather than as the error sspi makes of it.
async fn initialize<S>(
    sspi: &mut S,
    credentials: &mut S::CredentialsHandle,
    flags: ClientRequestFlags,
    spn: &str,
    input: &mut [SecurityBuffer],
) -> Result<(SecurityStatus, Vec<u8>), WinrmError>
where
    S: Sspi + SspiImpl,
{
    let mut output = vec![SecurityBuffer::new(Vec::new(), BufferType::Token)];
    let mut failure = None;
    let result = {
        let mut builder = (sspi.initialize_security_context())
            .with_credentials_handle(credentials)
            .with_context_requirements(flags)
            .with_target_data_representation(DataRepresentation::Native)
            .with_target_name(spn)
            .with_input(input)
            .with_output(&mut output);
        let mut generator = sspi.initialize_security_context_impl(&mut builder)?;
        let mut state = generator.start();
        loop {
            match state {
                GeneratorState::Suspended(request) => {
                    let reply = kerberos::exchange(&request).await.map_err(|error| {
                        let unreachable = sspi::ErrorKind::NoAuthenticatingAuthority;
                        let reported = sspi::Error::new(unreachable, error.to_string());
                        failure = Some(error);
                        reported
                    });
                    state = generator.resume(reply);
                }
                GeneratorState::Completed(result) => break result,
            }
        }
    };
    match (result, failure) {
        (Err(_), Some(failure)) => Err(failure),
        (result, _) => Ok((result?.status, output.swap_remove(0).buffer)),
    }
}

fn seal(sspi: &mut impl Sspi, message: &[u8]) -> Result<Vec<u8>, WinrmError> {
    let trailer = usize::try_from(sspi.query_context_sizes()?.security_trailer)
        .expect("a trailer is a few bytes");
    let (mut signature, mut data) = (vec![0; trailer], message.to_vec());
    let mut buffers = [
        SecurityBufferRef::token_buf(&mut signature),
        SecurityBufferRef::data_buf(&mut data),
    ];
    sspi.encrypt_message(EncryptionFlags::empty(), &mut buffers)?;
    let sealed = Sealed {
        length: message.len(),
        signature: buffers[0].data(),
        sealed: buffers[1].data(),
    };
    Ok(sealed.frame())
}

fn unseal(sspi: &mut impl Sspi, framed: &Sealed<'_>) -> Result<Vec<u8>, WinrmError> {
    let (mut signature, mut data) = (framed.signature.to_vec(), framed.sealed.to_vec());
    let mut buffers = [
        SecurityBufferRef::token_buf(&mut signature),
        SecurityBufferRef::data_buf(&mut data),
    ];
    sspi.decrypt_message(&mut buffers)?;
    Ok(buffers[1].data().to_vec())
}
