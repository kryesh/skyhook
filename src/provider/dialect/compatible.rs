//! Standards-following servers, which need only an endpoint. Each placement
//! dimension may still be selected in the entry, and any codec is admitted.

use serde::{Deserialize, Serialize};

use super::{
    BuildError, Common, Connection, Dialect, DialectConfig, DialectError, Overrides, Pending,
    Profile, Scheme, key,
};
use crate::provider::{
    codec::CodecName,
    http::{Headers, Transport},
};

pub use super::OverrideError as Error;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Config(pub Overrides);

impl DialectConfig for Config {
    fn admit(&self, _: &Common, codec: CodecName) -> Result<Profile, DialectError> {
        let codec = self.0.apply(super::base(codec))?;
        Ok(Profile::new(codec, Transport::plain(), Dialect::Compatible))
    }

    /// A key travels as the codec's own.
    fn credentials(
        &self,
        profile: &Profile,
        connection: &Connection,
    ) -> Result<Headers<Pending>, BuildError> {
        Ok(key(connection, Scheme::standard(profile.codec.name())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{dialect::tests::head, http::transport::tests::header_values};

    #[tokio::test]
    async fn a_key_travels_as_the_codec_expects() {
        let messages = head(&Config::default(), CodecName::Messages, "k").await;
        assert_eq!(header_values(&messages, "x-api-key"), ["k"]);
        assert_eq!(
            header_values(&messages, "anthropic-version"),
            ["2023-06-01"]
        );
        assert!(header_values(&messages, "authorization").is_empty());
        let chat = head(&Config::default(), CodecName::ChatCompletions, "k").await;
        assert_eq!(header_values(&chat, "authorization"), ["Bearer k"]);
        assert!(header_values(&chat, "x-api-key").is_empty());
    }
}
