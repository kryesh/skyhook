//! Standards-following servers, which need only an endpoint. Each placement
//! dimension may still be selected in the entry, and any codec is admitted.

use super::{Dialect, DialectConfig, DialectError, Profile, Scheme};
use crate::provider::{
    codec::{Codec, CodecName, chat_completions, messages, responses},
    http::Transport,
};

/// Provider-only options; request settings are declared separately.
pub type Options = crate::provider::settings::Empty;

crate::provider::settings::settings! {
    #[derive(Default)]
    pub struct Config => Patch {}
}

impl DialectConfig for Config {
    /// A key travels as the codec's own.
    fn admit(&self, codec: CodecName) -> Result<Profile, DialectError> {
        let conventions = base(codec);
        Ok(Profile {
            key: Scheme::standard(codec),
            ..Profile::new(conventions, Transport::plain(), Dialect::Compatible)
        })
    }
}

/// The base conventions of a family, which `compatible` serves as they are.
pub(crate) fn base(codec: CodecName) -> Codec {
    match codec {
        CodecName::ChatCompletions => {
            Codec::ChatCompletions(chat_completions::Dialect::compatible())
        }
        CodecName::Responses => Codec::Responses(responses::Dialect::stateless()),
        CodecName::Messages => Codec::Messages(messages::Dialect::anthropic()),
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
