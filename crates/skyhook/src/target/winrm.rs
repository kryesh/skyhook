//! A target's WinRM settings.
use std::num::NonZeroU16;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook_worker::{
    origin::winrm::{Auth, AuthKind, Kdc, PrincipalError, Tls},
    secret::Credential,
};

use super::Password;

/// How a WinRM target authenticates, which listener it uses and what it trusts there.
/// `auth` and `tls` are independent.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "RawWinrmOptions", into = "RawWinrmOptions")]
pub struct WinrmOptions {
    pub auth: Auth<Password>,
    pub tls: Tls,
    pub port: Option<NonZeroU16>,
}

// `WinrmOptions` as written.
#[serde_with::skip_serializing_none]
#[derive(Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields, rename = "WinrmOptions")]
#[schemars(rename = "WinrmOptions")]
struct RawWinrmOptions {
    /// Remote user; with kerberos, user@REALM or REALM\user, whose realm issues the ticket
    user: String,
    /// Read on the connection's origin at each connection
    #[schemars(with = "Credential")]
    password: Password,
    #[serde(default = "default_auth")]
    auth: AuthKind,
    #[serde(default = "default_tls")]
    tls: Tls,
    /// WinRM port; defaults to 5986 with TLS and 5985 with tls off
    port: Option<NonZeroU16>,
    /// With kerberos, the KDC as host or host:port; defaults to the one the realm's DNS SRV records name
    kdc: Option<Kdc>,
}

const fn default_auth() -> AuthKind {
    AuthKind::Negotiate
}

const fn default_tls() -> Tls {
    Tls::System
}

/// Why written WinRM settings name no way to authenticate.
#[derive(Debug, thiserror::Error)]
enum WinrmOptionsError {
    #[error("user must be nonempty without control characters")]
    User,
    #[error(transparent)]
    Principal(#[from] PrincipalError),
    #[error("kdc applies only to kerberos")]
    Kdc,
}

impl TryFrom<RawWinrmOptions> for WinrmOptions {
    type Error = WinrmOptionsError;

    fn try_from(raw: RawWinrmOptions) -> Result<Self, Self::Error> {
        let RawWinrmOptions {
            user,
            password,
            auth,
            tls,
            port,
            kdc,
        } = raw;
        if user.is_empty() || user.chars().any(char::is_control) {
            return Err(WinrmOptionsError::User);
        }
        let auth = match (auth, kdc) {
            (AuthKind::Kerberos, kdc) => Auth::Kerberos {
                principal: user.parse()?,
                password,
                kdc,
            },
            (_, Some(_)) => return Err(WinrmOptionsError::Kdc),
            (AuthKind::Negotiate, None) => Auth::Negotiate { user, password },
            (AuthKind::Basic, None) => Auth::Basic { user, password },
        };
        Ok(Self { auth, tls, port })
    }
}

impl From<WinrmOptions> for RawWinrmOptions {
    fn from(options: WinrmOptions) -> Self {
        let WinrmOptions { auth, tls, port } = options;
        Self {
            user: auth.user(),
            password: auth.password().clone(),
            auth: auth.kind(),
            kdc: auth.kdc().cloned(),
            tls,
            port,
        }
    }
}

/// Schemas describe the fields as written.
impl JsonSchema for WinrmOptions {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        RawWinrmOptions::schema_name()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        RawWinrmOptions::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The user, `auth` and `kdc` name one way to authenticate: Kerberos takes its realm
    /// from the user, and only Kerberos takes a KDC; every transport needs a password,
    /// and `tls: off` and `tls: insecure` are words, not YAML 1.1's false. A user and
    /// password alone are NTLM over HTTPS with the system's trust.
    #[test]
    fn winrm_settings_name_one_way_to_authenticate() {
        let parse = |text: &str| crate::yaml::parse::<WinrmOptions>(text);
        let minimal = parse("{user: alice, password: pw}").unwrap();
        assert_eq!(
            (minimal.auth.kind(), minimal.tls),
            (AuthKind::Negotiate, Tls::System)
        );
        let insecure = parse("{user: alice, password: pw, tls: insecure}").unwrap();
        assert_eq!(insecure.tls, Tls::Insecure);
        let options = parse(
            "{user: alice@corp.example, password: {env: PW}, auth: kerberos, kdc: dc.corp, tls: off}",
        )
        .unwrap();
        assert_eq!(options.tls, Tls::Off);
        let Auth::Kerberos { principal, kdc, .. } = &options.auth else {
            panic!("kerberos was configured: {options:?}");
        };
        assert_eq!(principal.realm(), "CORP.EXAMPLE");
        assert_eq!(
            kdc.as_ref().map(ToString::to_string).as_deref(),
            Some("dc.corp")
        );
        let fingerprint = "ab".repeat(32);
        let pinned = parse(&format!(
            "{{user: 'CORP\\alice', password: pw, auth: negotiate, tls: {{fingerprint: '{fingerprint}'}}, port: 15986}}"
        ))
        .unwrap();
        assert_eq!(pinned.auth.kind(), AuthKind::Negotiate);
        assert_eq!(pinned.tls, Tls::Fingerprint(fingerprint.parse().unwrap()));
        for invalid in [
            "{user: alice, password: pw, auth: kerberos, tls: system}",
            "{user: alice, password: pw, auth: basic, kdc: dc, tls: system}",
            "{user: alice, auth: negotiate, tls: system}",
            "{user: '', password: pw, auth: basic, tls: {ca: ~/ca.pem}}",
            "{user: alice, password: pw, auth: negotiate, tls: {fingerprint: ab}}",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
        // Written back as configured.
        let options = parse(
            "{user: 'CORP\\alice', password: {command: [pass]}, auth: kerberos, tls: {ca: ~/ca.pem}}",
        )
        .unwrap();
        let written = crate::yaml::to_string(&options).unwrap();
        assert_eq!(options.auth.user(), "CORP\\alice");
        assert_eq!(parse(&written).unwrap(), options);
    }
}
