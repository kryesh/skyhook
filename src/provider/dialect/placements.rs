//! The placement dimensions an entry or a model may select, as configuration
//! spells them, applied onto a family's conventions.

use serde::Serialize;

use crate::{
    named_enum::named_enum,
    provider::{
        codec::{
            BodyPath, CacheKey, Codec, CodecName, Locations, NonEmpty, chat_completions, messages,
            overlaps, responses,
        },
        http::Fields,
        settings::Nullable,
    },
};

/// Locations chosen in place of the preset, if any were: none for null.
fn chosen<T: Clone>(selection: &Nullable<NonEmpty<T>>) -> Option<Locations<T>> {
    match selection {
        Nullable::Default => None,
        Nullable::Null => Some(Locations::NONE),
        Nullable::Value(locations) => Some(locations.clone().into()),
    }
}

named_enum! {
    /// A placement dimension, as configuration spells it.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum PlacementKey {
        OutputLimit = "output_limit",
        ReasoningEffort = "reasoning_effort",
        ReasoningReplay = "reasoning_replay",
        ToolStream = "tool_stream",
        CacheKey = "cache_key",
        UserId = "user_id",
    }
}

/// A selection the codec has no dimension for, or one that would write where
/// something else already does.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("{key} is not a {family} setting")]
    Foreign {
        key: PlacementKey,
        family: CodecName,
    },
    #[error("{family} requires a field for {key}")]
    RequiresField {
        key: PlacementKey,
        family: CodecName,
    },
    #[error("{key} `{path}` is a field {family} requests already carry")]
    Reserved {
        key: PlacementKey,
        path: BodyPath,
        family: CodecName,
    },
    #[error(
        "{key} header `{name}` is reserved: `accept`, `content-type`, or one the {family} codec sends"
    )]
    ReservedHeader {
        key: PlacementKey,
        name: reqwest::header::HeaderName,
        family: CodecName,
    },
    #[error("{key} `{path}` overlaps {other} `{other_path}`")]
    Overlap {
        key: PlacementKey,
        path: BodyPath,
        other: PlacementKey,
        other_path: BodyPath,
    },
}

crate::provider::settings::settings! {
    /// Resolved placement selections: left out, each keeps the dialect preset; null
    /// places nothing; otherwise one location or a list of them.
    #[derive(Default)]
    pub struct Placements => PlacementsPatch {
        #[serde(default, skip_serializing_if = "Nullable::is_default")]
        pub output_limit: Nullable<NonEmpty<BodyPath>> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reasoning_effort: Option<NonEmpty<BodyPath>> => optional(NonEmpty<BodyPath>),
        /// Chat Completions only. A field keeps the dialect's replay shape.
        #[serde(default, skip_serializing_if = "Nullable::is_default")]
        pub reasoning_replay: Nullable<NonEmpty<BodyPath>> => default,
        /// Chat Completions only.
        #[serde(default, skip_serializing_if = "Nullable::is_default")]
        pub tool_stream: Nullable<NonEmpty<BodyPath>> => default,
        #[serde(default, skip_serializing_if = "Nullable::is_default")]
        pub cache_key: Nullable<NonEmpty<CacheKey>> => default,
        #[serde(default, skip_serializing_if = "Nullable::is_default")]
        pub user_id: Nullable<NonEmpty<BodyPath>> => default,
    }
}

impl Placements {
    /// The codec with these selections applied, or the first selection the
    /// codec has no dimension for or that withholds a value it needs.
    pub(crate) fn apply(&self, mut codec: Codec) -> Result<Codec, PlacementError> {
        let family = codec.name();
        if family != CodecName::ChatCompletions {
            for (key, selection) in [
                (PlacementKey::ReasoningReplay, &self.reasoning_replay),
                (PlacementKey::ToolStream, &self.tool_stream),
            ] {
                // Placing nothing is a selection every family can honour.
                if matches!(selection, Nullable::Value(_)) {
                    return Err(PlacementError::Foreign { key, family });
                }
            }
        }
        if let Some(effort) = &self.reasoning_effort {
            *codec.effort_mut() = effort.clone();
        }
        match &mut codec {
            Codec::ChatCompletions(dialect) => {
                if let Some(limit) = chosen(&self.output_limit) {
                    dialect.output_limit = limit;
                }
                match &self.reasoning_replay {
                    Nullable::Default => {}
                    Nullable::Null => {
                        dialect.reasoning_replay = chat_completions::ReasoningReplay::Unsupported;
                    }
                    Nullable::Value(fields) => {
                        dialect.reasoning_replay = dialect.reasoning_replay.at(fields.clone());
                    }
                }
                if let Some(stream) = chosen(&self.tool_stream) {
                    dialect.tool_stream = stream;
                }
            }
            Codec::Responses(dialect) => {
                if let Some(limit) = chosen(&self.output_limit) {
                    dialect.output_limit = limit;
                }
            }
            Codec::Messages(dialect) => match &self.output_limit {
                Nullable::Default => {}
                Nullable::Null => {
                    return Err(PlacementError::RequiresField {
                        key: PlacementKey::OutputLimit,
                        family,
                    });
                }
                Nullable::Value(fields) => {
                    dialect.output_limit = fields.clone();
                }
            },
        }
        let identity = codec.identity_mut();
        if let Some(keys) = chosen(&self.cache_key) {
            identity.cache_key = keys;
        }
        if let Some(user) = chosen(&self.user_id) {
            identity.user_id = user;
        }
        Ok(codec)
    }
}

/// Every placement the conventions select stays clear of the fields the codec
/// writes, of those the dialect adds, and of every other placement in the object
/// they share.
pub(crate) fn check(codec: &Codec, added: &Fields) -> Result<(), PlacementError> {
    use PlacementKey as Key;
    let family = codec.name();
    let mut body: Vec<(Key, &BodyPath)> = Vec::new();
    let mut message: Vec<(Key, &BodyPath)> = Vec::new();
    let at = |key: Key| move |path| (key, path);
    let (body_fields, message_fields) = match codec {
        Codec::ChatCompletions(dialect) => {
            body.extend(dialect.output_limit.iter().map(at(Key::OutputLimit)));
            body.extend(dialect.effort.iter().map(at(Key::ReasoningEffort)));
            body.extend(dialect.tool_stream.iter().map(at(Key::ToolStream)));
            let replay = dialect.reasoning_replay.paths();
            message.extend(replay.map(at(Key::ReasoningReplay)));
            (
                chat_completions::BODY_FIELDS,
                chat_completions::ASSISTANT_FIELDS,
            )
        }
        Codec::Responses(dialect) => {
            body.extend(dialect.output_limit.iter().map(at(Key::OutputLimit)));
            body.extend(dialect.effort.iter().map(at(Key::ReasoningEffort)));
            (responses::BODY_FIELDS, &[][..])
        }
        Codec::Messages(dialect) => {
            body.extend(dialect.output_limit.iter().map(at(Key::OutputLimit)));
            body.extend(dialect.effort.iter().map(at(Key::ReasoningEffort)));
            (messages::BODY_FIELDS, &[][..])
        }
    };
    let identity = codec.identity();
    for key in identity.cache_key.iter() {
        match key {
            CacheKey::Body(path) => body.push((Key::CacheKey, path)),
            CacheKey::Header(name) => {
                use reqwest::header::{ACCEPT, CONTENT_TYPE};
                if name == ACCEPT || name == CONTENT_TYPE || codec.headers().contains(name) {
                    let name = name.clone();
                    return Err(PlacementError::ReservedHeader {
                        key: Key::CacheKey,
                        name,
                        family,
                    });
                }
            }
        }
    }
    body.extend(identity.user_id.iter().map(at(Key::UserId)));
    let added = added.paths().map(BodyPath::as_str);
    let body_fields: Vec<_> = body_fields.iter().copied().chain(added).collect();
    for (fields, placements) in [(&body_fields[..], body), (message_fields, message)] {
        for (index, &(key, path)) in placements.iter().enumerate() {
            if fields.iter().any(|field| overlaps(field, path.as_str())) {
                let path = path.clone();
                return Err(PlacementError::Reserved { key, path, family });
            }
            if let Some(&(other, other_path)) = placements[index + 1..]
                .iter()
                .find(|(_, other)| overlaps(other.as_str(), path.as_str()))
            {
                return Err(PlacementError::Overlap {
                    key,
                    path: path.clone(),
                    other,
                    other_path: other_path.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        codec::{header, path},
        dialect::compatible::base,
    };

    #[test]
    fn selections_apply_per_family_and_reject_foreign_ones() {
        let placements: Placements = crate::yaml::parse(
            "output_limit: max_tokens\nreasoning_replay: null\ncache_key: {header: x-session-id}\nuser_id: metadata.user\n",
        )
        .unwrap();
        let Codec::ChatCompletions(chat) =
            placements.apply(base(CodecName::ChatCompletions)).unwrap()
        else {
            panic!()
        };
        assert_eq!(chat.output_limit, Locations::one(path("max_tokens")));
        assert_eq!(
            chat.reasoning_replay,
            chat_completions::ReasoningReplay::Unsupported
        );
        assert_eq!(
            chat.identity.cache_key,
            Locations::one(header("x-session-id"))
        );
        assert_eq!(chat.identity.user_id, Locations::one(path("metadata.user")));
        assert_eq!(chat.effort, NonEmpty::one(path("reasoning_effort")));
        // Placing no replay suits every family; naming a field only Chat.
        assert!(placements.apply(base(CodecName::Responses)).is_ok());
        let replay: Placements = crate::yaml::parse("reasoning_replay: reasoning").unwrap();
        assert_eq!(
            replay.apply(base(CodecName::Responses)).unwrap_err(),
            PlacementError::Foreign {
                key: PlacementKey::ReasoningReplay,
                family: CodecName::Responses
            }
        );
        let omitted = Placements {
            output_limit: Nullable::Null,
            ..Default::default()
        };
        assert_eq!(
            omitted.apply(base(CodecName::Messages)).unwrap_err(),
            PlacementError::RequiresField {
                key: PlacementKey::OutputLimit,
                family: CodecName::Messages
            }
        );
        let Codec::Responses(responses) = omitted.apply(base(CodecName::Responses)).unwrap() else {
            panic!()
        };
        assert_eq!(responses.output_limit, Locations::NONE);
        assert!(crate::yaml::parse::<PlacementsPatch>("reasoning_effort: null").is_err());
        assert!(crate::yaml::parse::<Placements>("cache_key: {header: 'bad header'}").is_err());
        assert!(crate::yaml::parse::<Placements>("unknown: 1").is_err());
        // `dump config` writes the tagged spelling back.
        assert_eq!(
            crate::yaml::to_string(&placements).unwrap().trim(),
            "output_limit: max_tokens\nreasoning_replay: null\ncache_key:\n  header: x-session-id\nuser_id: metadata.user"
        );
    }

    /// A list selects every location it names; locations in one list stay clear of
    /// one another, and a bad location is refused for its own reason.
    #[test]
    fn lists_place_one_value_at_several_locations() {
        let text = "output_limit: [max_tokens, max_completion_tokens]\n\
            reasoning_effort: [reasoning_effort, reasoning.effort]\n\
            reasoning_replay: [reasoning, reasoning_content]\n\
            cache_key: [{header: x-session-id}, {body: prompt_cache_key}]\n";
        let placements: Placements = crate::yaml::parse(text).unwrap();
        let written = crate::yaml::to_string(&placements).unwrap();
        assert_eq!(
            crate::yaml::parse::<Placements>(&written).unwrap(),
            placements
        );
        let applied = placements.apply(base(CodecName::ChatCompletions)).unwrap();
        assert_eq!(check(&applied, &Fields::default()), Ok(()));
        let Codec::ChatCompletions(chat) = applied else {
            panic!()
        };
        let both = |a, b| Locations::one(path(a)).with(path(b));
        assert_eq!(
            chat.output_limit,
            both("max_tokens", "max_completion_tokens")
        );
        assert_eq!(
            Locations::from(chat.effort),
            both("reasoning_effort", "reasoning.effort")
        );
        let chat_completions::ReasoningReplay::Text(replay) = chat.reasoning_replay else {
            panic!("{:?}", chat.reasoning_replay)
        };
        assert_eq!(
            Locations::from(replay),
            both("reasoning", "reasoning_content")
        );
        assert_eq!(
            chat.identity.cache_key,
            Locations::one(header("x-session-id")).with(CacheKey::Body(path("prompt_cache_key")))
        );
        let nested: Placements =
            crate::yaml::parse("output_limit: [limits, limits.output]").unwrap();
        assert_eq!(
            check(
                &nested.apply(base(CodecName::Responses)).unwrap(),
                &Fields::default()
            ),
            Err(PlacementError::Overlap {
                key: PlacementKey::OutputLimit,
                path: path("limits"),
                other: PlacementKey::OutputLimit,
                other_path: path("limits.output"),
            })
        );
        let refused = crate::yaml::parse::<Placements>("cache_key: [{header: 'bad header'}]");
        assert!(
            refused
                .unwrap_err()
                .to_string()
                .contains("not a valid header name")
        );
    }

    #[test]
    fn a_replay_field_keeps_the_dialect_shape() {
        use crate::provider::codec::UnsignedReasoning;
        use chat_completions::{NativeReplay, ReasoningReplay};
        let field: Placements = crate::yaml::parse("reasoning_replay: kept").unwrap();
        let native = |objects, unsigned| NativeReplay {
            objects: NonEmpty::one(path(objects)),
            unsigned,
        };
        let plain = || UnsignedReasoning::Replayed(Locations::one(path("reasoning")));
        for (preset, expected) in [
            (
                ReasoningReplay::ThinkingBlocks(native(
                    "thinking_blocks",
                    UnsignedReasoning::Dropped,
                )),
                ReasoningReplay::ThinkingBlocks(native("kept", UnsignedReasoning::Dropped)),
            ),
            (
                ReasoningReplay::Details(native("reasoning_details", plain())),
                ReasoningReplay::Details(native("kept", plain())),
            ),
            (
                ReasoningReplay::Unsupported,
                ReasoningReplay::Text(NonEmpty::one(path("kept"))),
            ),
        ] {
            let codec = Codec::ChatCompletions(chat_completions::Dialect {
                reasoning_replay: preset,
                ..chat_completions::Dialect::compatible()
            });
            let Codec::ChatCompletions(chat) = field.apply(codec).unwrap() else {
                panic!()
            };
            assert_eq!(chat.reasoning_replay, expected);
        }
    }

    #[test]
    fn placements_stay_clear_of_codec_fields_and_one_another() {
        use crate::provider::dialect::{
            DialectConfig, anthropic, codex, compatible, litellm, openai, openrouter,
        };
        let litellm = |upstream| litellm::Config {
            upstream,
            tags: Vec::new(),
            state_role: None,
        };
        let presets: [&dyn DialectConfig; 8] = [
            &compatible::Config::default(),
            &openai::Config::default(),
            &anthropic::Config::default(),
            &codex::Config::default(),
            &openrouter::Config::default(),
            &litellm(litellm::Upstream::Openai),
            &litellm(litellm::Upstream::Anthropic),
            &litellm(litellm::Upstream::Bedrock),
        ];
        for (index, settings) in presets.into_iter().enumerate() {
            for family in [
                CodecName::ChatCompletions,
                CodecName::Responses,
                CodecName::Messages,
            ] {
                if let Ok(profile) = settings.admit(family) {
                    assert_eq!(
                        check(&profile.codec, &profile.transport.fields),
                        Ok(()),
                        "preset {index} {family}"
                    );
                }
            }
        }
        let checked = |family, text: &str| {
            let placements: Placements = crate::yaml::parse(text).unwrap();
            check(&placements.apply(base(family)).unwrap(), &Fields::default())
        };
        let chat = CodecName::ChatCompletions;
        let header = |name: &'static str, family| PlacementError::ReservedHeader {
            key: PlacementKey::CacheKey,
            name: reqwest::header::HeaderName::from_static(name),
            family,
        };
        assert_eq!(
            checked(chat, "cache_key: {header: Accept}"),
            Err(header("accept", chat))
        );
        let messages = CodecName::Messages;
        for text in [
            "cache_key: {header: anthropic-version}",
            "cache_key: [{header: x-session}, {header: anthropic-version}]",
        ] {
            assert_eq!(
                checked(messages, text),
                Err(header("anthropic-version", messages))
            );
        }
        assert_eq!(checked(chat, "cache_key: {header: x-session}"), Ok(()));
        // Paths written apart, and a replay key named like a body field, pass.
        assert_eq!(
            checked(
                chat,
                "user_id: metadata.user_id\ncache_key: {body: metadata.session}\nreasoning_replay: reasoning_effort"
            ),
            Ok(())
        );
        let reserved = |key, path: &str, family| PlacementError::Reserved {
            key,
            path: BodyPath::new(path).unwrap(),
            family,
        };
        assert_eq!(
            checked(chat, "reasoning_effort: model"),
            Err(reserved(PlacementKey::ReasoningEffort, "model", chat))
        );
        assert_eq!(
            checked(chat, "reasoning_replay: tool_calls.0"),
            Err(reserved(
                PlacementKey::ReasoningReplay,
                "tool_calls.0",
                chat
            ))
        );
        // The codec writes `reasoning.summary`, so `reasoning` is taken.
        let responses = CodecName::Responses;
        assert_eq!(
            checked(responses, "reasoning_effort: reasoning"),
            Err(reserved(
                PlacementKey::ReasoningEffort,
                "reasoning",
                responses
            ))
        );
        let path = "access_programs.cyber";
        for (key, selection) in [
            (
                PlacementKey::ReasoningEffort,
                format!("reasoning_effort: {path}"),
            ),
            (
                PlacementKey::CacheKey,
                format!("cache_key: {{body: {path}}}"),
            ),
        ] {
            assert_eq!(
                checked(responses, &selection),
                Err(reserved(key, path, responses))
            );
        }
        let overlap = |key, path: &str, other, other_path: &str| PlacementError::Overlap {
            key,
            path: BodyPath::new(path).unwrap(),
            other,
            other_path: BodyPath::new(other_path).unwrap(),
        };
        assert_eq!(
            checked(chat, "output_limit: reasoning_effort"),
            Err(overlap(
                PlacementKey::OutputLimit,
                "reasoning_effort",
                PlacementKey::ReasoningEffort,
                "reasoning_effort"
            ))
        );
        assert_eq!(
            checked(
                CodecName::Messages,
                "user_id: metadata\ncache_key: {body: metadata.session}"
            ),
            Err(overlap(
                PlacementKey::CacheKey,
                "metadata.session",
                PlacementKey::UserId,
                "metadata"
            ))
        );
    }
}
