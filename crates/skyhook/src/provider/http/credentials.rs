//! Configured header values: admitted without reading anything, pending while a
//! provider or connection is built, and read once, by each build, when
//! composition shows they are sent.

use std::fmt;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

use super::headers::{Headers, ProducedValue, Value, header_value, prefixed};
use crate::provider::{ProviderError, codec::CodecName};
use skyhook_worker::secret::{Credential, CredentialKind, ResolveError, SourceProblem};

/// The setting a configured value belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueField {
    ApiKey,
    Header(HeaderName),
}

impl fmt::Display for ValueField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey => f.write_str("api_key"),
            Self::Header(name) => write!(f, "headers.{name}"),
        }
    }
}

/// A configured value that cannot be used: refused when the entry is admitted,
/// when the provider is built, or when a command or file is read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub struct ValueError {
    pub field: ValueField,
    pub problem: ValueProblem,
}

/// `api_key: env KEY is not a valid header value`, but `api_key from env KEY: not set`.
impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.problem {
            ValueProblem::Unreadable(error) => write!(f, "{} {error}", self.field),
            problem => write!(f, "{}: {problem}", self.field),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValueProblem {
    #[error(transparent)]
    Source(#[from] SourceProblem),
    /// Named by its source alone: the credential is redacted.
    #[error("{0} is not a valid header value")]
    InvalidHeaderValue(Credential),
    #[error(transparent)]
    Unreadable(#[from] ResolveError),
}

/// Check the credential `field` is configured with, read on this host, without reading
/// the environment, running a command or opening a file; a literal must already be a
/// header value.
pub(crate) fn admit(credential: &Credential, field: ValueField) -> Result<(), ValueError> {
    let problem = match credential.admit(skyhook_worker::target::Platform::current().os) {
        Err(source) => ValueProblem::Source(source),
        Ok(())
            if credential
                .literal()
                .is_some_and(|text| header_value(text).is_none()) =>
        {
            ValueProblem::InvalidHeaderValue(credential.redacted())
        }
        Ok(()) => return Ok(()),
    };
    Err(ValueError { field, problem })
}

/// `credential`'s value for `field`, behind `prefix` as `Bearer ` precedes a bearer
/// token, once composition shows it is sent.
pub(crate) fn pending(
    credential: &Credential,
    field: ValueField,
    prefix: Option<&'static str>,
) -> Pending {
    Pending::Credential {
        credential: credential.clone(),
        field,
        prefix,
    }
}

/// A header value while a provider is built: ready, or a credential read once
/// composition shows the value is sent.
#[derive(Clone)]
pub(crate) enum Pending {
    Ready(Value),
    Credential {
        credential: Credential,
        field: ValueField,
        prefix: Option<&'static str>,
    },
}

/// The credentials one provider build has read, shared by all its models so that
/// each reads the environment once and shares one command generation, whatever
/// prefix its credential scheme puts in front.
#[derive(Default)]
pub(crate) struct Sources(Vec<(ValueField, Raw)>);

/// A credential's value before any prefix.
#[derive(Clone)]
enum Raw {
    Fixed(HeaderValue),
    Produced(ProducedValue),
}

impl Sources {
    pub(crate) fn read(&mut self, pending: Pending) -> Result<Value, ValueError> {
        let (credential, field, prefix) = match pending {
            Pending::Ready(value) => return Ok(value),
            Pending::Credential {
                credential,
                field,
                prefix,
            } => (credential, field, prefix),
        };
        let raw = match self.0.iter().find(|(read, _)| *read == field) {
            Some((_, raw)) => raw.clone(),
            None => {
                let raw = read(credential, &field)?;
                self.0.push((field, raw.clone()));
                raw
            }
        };
        Ok(match raw {
            Raw::Fixed(value) => Value::Fixed(prefixed(prefix, &value)),
            Raw::Produced(value) => Value::Produced(value.with_prefix(prefix)),
        })
    }
}

/// Credentials are never logged, whatever their source; other values only when
/// written literally in the file. What reads without running anything is read
/// here; commands and files stay lazy.
fn read(credential: Credential, field: &ValueField) -> Result<Raw, ValueError> {
    let Some(read) = credential.read_now() else {
        return Ok(Raw::Produced(ProducedValue::new(credential, field.clone())));
    };
    let problem = |problem| ValueError {
        field: field.clone(),
        problem,
    };
    let text = read.map_err(|error| problem(error.into()))?;
    let invalid = || problem(ValueProblem::InvalidHeaderValue(credential.redacted()));
    let mut value = header_value(&text).ok_or_else(invalid)?;
    value.set_sensitive(
        *field == ValueField::ApiKey || credential.kind() != CredentialKind::Literal,
    );
    Ok(Raw::Fixed(value))
}

/// Why a configured header was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("`{0}` is not a valid header name")]
    Name(String),
    #[error("`{0}` is named twice; header names ignore case")]
    Duplicate(HeaderName),
    #[error("{}", ValueError { field: ValueField::Header(name.clone()), problem: problem.clone() })]
    Value {
        name: HeaderName,
        problem: ValueProblem,
    },
}

impl HeaderError {
    /// The header refused, and why, as said at that header.
    pub(crate) fn refusal(&self) -> (&str, String) {
        match self {
            Self::Name(name) => (name, self.to_string()),
            Self::Duplicate(name) => (name.as_str(), self.to_string()),
            Self::Value { name, problem } => (name.as_str(), problem.to_string()),
        }
    }
}

/// Configured `headers`, admitted: distinct valid names, each with its credential.
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(into = "Configured")]
pub(crate) struct ConfiguredHeaders(Vec<(HeaderName, Credential)>);

/// Each refusal at the header it names.
impl skyhook_base::decode::Decode for ConfiguredHeaders {
    fn decode(
        node: &skyhook_base::decode::Node,
        cx: &mut skyhook_base::decode::Context,
    ) -> Option<Self> {
        let headers = Configured::decode(node, cx)?;
        Self::admit(&headers)
            .map_err(|error| {
                let (name, message) = error.refusal();
                let path = vec![skyhook_base::decode::Segment::Key(name.to_owned())];
                cx.error_within(node, path, message);
            })
            .ok()
    }
}

type Configured = indexmap::IndexMap<String, Credential>;

/// The headers as configured.
impl From<ConfiguredHeaders> for Configured {
    fn from(headers: ConfiguredHeaders) -> Self {
        (headers.0.into_iter())
            .map(|(name, credential)| (name.as_str().to_owned(), credential))
            .collect()
    }
}

impl ConfiguredHeaders {
    /// Check names and sources without reading the environment or running a command.
    pub(crate) fn admit<'a>(
        headers: impl IntoIterator<Item = (&'a String, &'a Credential)>,
    ) -> Result<Self, HeaderError> {
        let mut admitted: Vec<(HeaderName, Credential)> = Vec::new();
        for (name, credential) in headers {
            let header = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| HeaderError::Name(name.clone()))?;
            if admitted.iter().any(|(held, _)| *held == header) {
                return Err(HeaderError::Duplicate(header));
            }
            admit(credential, ValueField::Header(header.clone())).map_err(|error| {
                let (name, problem) = (header.clone(), error.problem);
                HeaderError::Value { name, problem }
            })?;
            admitted.push((header, credential.clone()));
        }
        Ok(Self(admitted))
    }

    /// The headers, each read once composition shows it is sent.
    pub(crate) fn pending(&self) -> Headers<Pending> {
        let mut headers = Headers::default();
        for (header, credential) in &self.0 {
            let field = ValueField::Header(header.clone());
            headers.insert(header.clone(), pending(credential, field, None));
        }
        headers
    }
}

/// Why an admitted entry's provider could not be constructed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Value(#[from] ValueError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// How a key travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scheme {
    /// `authorization: Bearer <key>`.
    Bearer,
    /// `x-api-key: <key>`.
    XApiKey,
}

impl Scheme {
    /// The family's own convention: Messages takes `x-api-key`, the OpenAI
    /// families a bearer token.
    pub(crate) fn standard(codec: CodecName) -> Self {
        match codec {
            CodecName::Messages => Self::XApiKey,
            CodecName::ChatCompletions | CodecName::Responses => Self::Bearer,
        }
    }

    /// The configured `api_key` in the header this scheme sends it in.
    pub(crate) fn credential(self, key: &Credential) -> Headers<Pending> {
        let (header, prefix) = match self {
            Self::Bearer => (AUTHORIZATION, Some("Bearer ")),
            Self::XApiKey => (HeaderName::from_static("x-api-key"), None),
        };
        let mut headers = Headers::default();
        headers.insert(header, pending(key, ValueField::ApiKey, prefix));
        headers
    }
}

impl Headers<Pending> {
    /// Read every value as one request sends it, with credentials of its own.
    pub(crate) async fn read(self) -> Result<HeaderMap, BuildError> {
        let mut sources = Sources::default();
        let headers = self.try_map(|pending| sources.read(pending))?;
        Ok(headers.resolve().await?.map())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[test]
    fn literals_are_header_values_and_credentials_stay_sensitive() {
        let parse = |text: &str| crate::yaml::parse::<Credential>(text).unwrap();
        let problem = |text| {
            admit(&parse(text), ValueField::ApiKey)
                .err()
                .map(|e| e.problem)
        };
        assert_matches!(
            problem("{env: A=B}"),
            Some(ValueProblem::Source(SourceProblem::InvalidEnvName))
        );
        // A block scalar keeps its trailing newline, which no header carries.
        assert_eq!(
            problem("|\n  sk-key\n"),
            Some(ValueProblem::InvalidHeaderValue(Credential::Literal(
                "literal".into()
            )))
        );
        let fixed = |text: &str, field: ValueField, prefix| {
            let credential = parse(text);
            admit(&credential, field.clone()).unwrap();
            match Sources::default()
                .read(pending(&credential, field, prefix))
                .unwrap()
            {
                Value::Fixed(value) => value,
                _ => panic!("{text} is read when the provider is built"),
            }
        };
        let header = || ValueField::Header(HeaderName::from_static("x-header"));
        let value = fixed("k", ValueField::ApiKey, Some("Bearer "));
        assert_eq!(value.to_str().unwrap(), "Bearer k");
        assert!(value.is_sensitive());
        assert!(!fixed("v", header(), None).is_sensitive());
        // Environment values stay sensitive.
        assert!(fixed("{env: PATH}", header(), None).is_sensitive());
    }
}
