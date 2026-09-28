//! The placement dimensions an entry or a model may select, as configuration
//! spells them, applied onto a family's conventions.

use serde::{Deserialize, Serialize};

use crate::{
    named_enum::named_enum,
    provider::codec::{
        BodyPath, CacheKey, Codec, CodecName, chat_completions, messages, overlaps, responses,
    },
};

/// A body placement as configuration spells it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Omitted,
    Field(BodyPath),
}

impl Placement {
    fn path(&self) -> Option<BodyPath> {
        match self {
            Self::Omitted => None,
            Self::Field(path) => Some(path.clone()),
        }
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
    #[error("{key} `{path}` is a field the {family} codec writes")]
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
    /// Resolved placement selections. None keeps the dialect preset; a patch
    /// can explicitly reset an inherited selection to None.
    #[derive(Default)]
    pub struct Placements => PlacementsPatch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub output_limit: Option<Placement> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reasoning_effort: Option<BodyPath> => default,
        /// Chat Completions only. A field keeps the dialect's replay shape.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reasoning_replay: Option<Placement> => default,
        /// Chat Completions only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tool_stream: Option<Placement> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cache_key: Option<CacheKey> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub user_id: Option<Placement> => default,
    }
}

impl Placements {
    /// The codec with these selections applied, or the first selection the
    /// codec has no dimension for.
    pub(crate) fn apply(&self, mut codec: Codec) -> Result<Codec, PlacementError> {
        let family = codec.name();
        if family != CodecName::ChatCompletions {
            for (key, selected) in [
                (
                    PlacementKey::ReasoningReplay,
                    self.reasoning_replay.is_some(),
                ),
                (PlacementKey::ToolStream, self.tool_stream.is_some()),
            ] {
                if selected {
                    return Err(PlacementError::Foreign { key, family });
                }
            }
        }
        if let Some(effort) = &self.reasoning_effort {
            codec.effort_mut().path = effort.clone();
        }
        match &mut codec {
            Codec::ChatCompletions(dialect) => {
                if let Some(limit) = &self.output_limit {
                    dialect.output_limit = limit.path();
                }
                if let Some(replay) = &self.reasoning_replay {
                    dialect.reasoning_replay = match replay.path() {
                        Some(path) => dialect.reasoning_replay.at(path),
                        None => chat_completions::ReasoningReplay::Unsupported,
                    };
                }
                if let Some(stream) = &self.tool_stream {
                    dialect.tool_stream = stream.path();
                }
            }
            Codec::Responses(dialect) => {
                if let Some(limit) = &self.output_limit {
                    dialect.output_limit = limit.path();
                }
            }
            Codec::Messages(dialect) => {
                if let Some(limit) = &self.output_limit {
                    dialect.output_limit = limit.path().ok_or(PlacementError::RequiresField {
                        key: PlacementKey::OutputLimit,
                        family,
                    })?;
                }
            }
        }
        let identity = codec.identity_mut();
        if let Some(key) = &self.cache_key {
            identity.cache_key = key.clone();
        }
        if let Some(user) = &self.user_id {
            identity.user_id = user.path();
        }
        Ok(codec)
    }
}

/// Every placement the conventions select stays clear of the fields the codec
/// writes and of every other placement in the object they share.
pub(crate) fn check(codec: &Codec) -> Result<(), PlacementError> {
    use PlacementKey as Key;
    let family = codec.name();
    let mut body: Vec<(Key, &BodyPath)> = Vec::new();
    let mut message: Vec<(Key, &BodyPath)> = Vec::new();
    let (body_fields, message_fields) = match codec {
        Codec::ChatCompletions(dialect) => {
            body.extend(
                dialect
                    .output_limit
                    .iter()
                    .map(|path| (Key::OutputLimit, path)),
            );
            body.push((Key::ReasoningEffort, &dialect.effort.path));
            body.extend(
                dialect
                    .tool_stream
                    .iter()
                    .map(|path| (Key::ToolStream, path)),
            );
            message.extend(
                dialect
                    .reasoning_replay
                    .path()
                    .map(|path| (Key::ReasoningReplay, path)),
            );
            (
                chat_completions::BODY_FIELDS,
                chat_completions::ASSISTANT_FIELDS,
            )
        }
        Codec::Responses(dialect) => {
            body.extend(
                dialect
                    .output_limit
                    .iter()
                    .map(|path| (Key::OutputLimit, path)),
            );
            body.push((Key::ReasoningEffort, &dialect.effort.path));
            (responses::BODY_FIELDS, &[][..])
        }
        Codec::Messages(dialect) => {
            body.push((Key::OutputLimit, &dialect.output_limit));
            body.push((Key::ReasoningEffort, &dialect.effort.path));
            (messages::BODY_FIELDS, &[][..])
        }
    };
    let identity = codec.identity();
    match &identity.cache_key {
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
        CacheKey::Omitted => {}
    }
    body.extend(identity.user_id.iter().map(|path| (Key::UserId, path)));
    for (fields, placements) in [(body_fields, body), (message_fields, message)] {
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
    fn null_resets_an_inherited_selection_but_omitted_is_a_value() {
        use crate::provider::settings::{Patch as _, Settings};

        let lower: PlacementsPatch =
            crate::yaml::parse("output_limit: {field: max_tokens}").unwrap();
        let omitted: PlacementsPatch = crate::yaml::parse("output_limit: omitted").unwrap();
        assert_eq!(
            Placements::resolve(&lower.overlay(&omitted))
                .unwrap()
                .output_limit,
            Some(Placement::Omitted)
        );
        let reset: PlacementsPatch = crate::yaml::parse("output_limit: null").unwrap();
        assert_eq!(
            crate::yaml::to_string(&reset).unwrap(),
            "output_limit: null\n"
        );
        let resolved = Placements::resolve(&lower.overlay(&reset)).unwrap();
        assert_eq!(resolved.output_limit, None);
        assert_eq!(
            Placements::resolve(&lower.overlay(&PlacementsPatch::default()))
                .unwrap()
                .output_limit,
            Some(Placement::Field(path("max_tokens")))
        );
    }

    #[test]
    fn selections_apply_per_family_and_reject_foreign_ones() {
        let placements: Placements = crate::yaml::parse(
            "output_limit: {field: max_tokens}\nreasoning_replay: omitted\ncache_key: {header: x-session-id}\nuser_id: {field: metadata.user}\n",
        )
        .unwrap();
        let Codec::ChatCompletions(chat) =
            placements.apply(base(CodecName::ChatCompletions)).unwrap()
        else {
            panic!()
        };
        assert_eq!(chat.output_limit, Some(path("max_tokens")));
        assert_eq!(
            chat.reasoning_replay,
            chat_completions::ReasoningReplay::Unsupported
        );
        assert_eq!(chat.identity.cache_key, header("x-session-id"));
        assert_eq!(chat.identity.user_id, Some(path("metadata.user")));
        assert_eq!(chat.effort.path, path("reasoning_effort"));
        assert_eq!(
            placements.apply(base(CodecName::Responses)).unwrap_err(),
            PlacementError::Foreign {
                key: PlacementKey::ReasoningReplay,
                family: CodecName::Responses
            }
        );
        let omitted = Placements {
            output_limit: Some(Placement::Omitted),
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
        assert_eq!(responses.output_limit, None);
        assert!(crate::yaml::parse::<Placements>("cache_key: {header: 'bad header'}").is_err());
        assert!(crate::yaml::parse::<Placements>("unknown: 1").is_err());
        // `dump config` writes the tagged spelling back.
        assert_eq!(
            crate::yaml::to_string(&placements).unwrap().trim(),
            "output_limit:\n  field: max_tokens\nreasoning_replay: omitted\ncache_key:\n  header: x-session-id\nuser_id:\n  field: metadata.user"
        );
    }

    #[test]
    fn a_replay_field_keeps_the_dialect_shape() {
        use chat_completions::ReasoningReplay;
        let field: Placements = crate::yaml::parse("reasoning_replay: {field: kept}").unwrap();
        for (preset, expected) in [
            (
                ReasoningReplay::ThinkingBlocks(path("thinking_blocks")),
                ReasoningReplay::ThinkingBlocks(path("kept")),
            ),
            (
                ReasoningReplay::Details(path("reasoning_details")),
                ReasoningReplay::Details(path("kept")),
            ),
            (
                ReasoningReplay::Unsupported,
                ReasoningReplay::Text(path("kept")),
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
        };
        let presets: [&dyn DialectConfig; 7] = [
            &compatible::Config::default(),
            &openai::Config::default(),
            &anthropic::Config::default(),
            &codex::Config::default(),
            &openrouter::Config::default(),
            &litellm(litellm::Upstream::Openai),
            &litellm(litellm::Upstream::Anthropic),
        ];
        for (index, settings) in presets.into_iter().enumerate() {
            for family in [
                CodecName::ChatCompletions,
                CodecName::Responses,
                CodecName::Messages,
            ] {
                if let Ok(profile) = settings.admit(family) {
                    assert_eq!(check(&profile.codec), Ok(()), "preset {index} {family}");
                }
            }
        }
        let checked = |family, text: &str| {
            let placements: Placements = crate::yaml::parse(text).unwrap();
            check(&placements.apply(base(family)).unwrap())
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
        assert_eq!(
            checked(messages, "cache_key: {header: anthropic-version}"),
            Err(header("anthropic-version", messages))
        );
        assert_eq!(checked(chat, "cache_key: {header: x-session}"), Ok(()));
        // Paths written apart, and a replay key named like a body field, pass.
        assert_eq!(
            checked(
                chat,
                "user_id: {field: metadata.user_id}\ncache_key: {body: metadata.session}\nreasoning_replay: {field: reasoning_effort}"
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
            checked(chat, "reasoning_replay: {field: tool_calls.0}"),
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
        let overlap = |key, path: &str, other, other_path: &str| PlacementError::Overlap {
            key,
            path: BodyPath::new(path).unwrap(),
            other,
            other_path: BodyPath::new(other_path).unwrap(),
        };
        assert_eq!(
            checked(chat, "output_limit: {field: reasoning_effort}"),
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
                "user_id: {field: metadata}\ncache_key: {body: metadata.session}"
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
