//! OpenAI Chat Completions and the compatible servers that speak it.

mod decoder;
mod dialect;
mod encoder;
mod wire;

pub(crate) use decoder::Decoder;
pub(crate) use dialect::{
    Dialect, EmptyContent, NativeReplay, ReasoningFormat, ReasoningReplay, SystemRole, UsageRequest,
};
pub(crate) use encoder::{ASSISTANT_FIELDS, BODY_FIELDS, encode};

use crate::provider::codec::{CodecName, common::Native};

const NATIVE: Native = Native(CodecName::ChatCompletions);
