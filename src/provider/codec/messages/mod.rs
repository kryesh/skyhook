//! Native Anthropic Messages wire codec.
mod decoder;
mod dialect;
mod encoder;
mod errors;
mod native;

use crate::provider::codec::{CodecName, common::Native};

pub(crate) use decoder::Decoder;
pub(crate) use dialect::{BETA_HEADER, Dialect, SystemState, ThinkingBinding};
pub(crate) use encoder::{BODY_FIELDS, encode};
pub(crate) use errors::Code;

const NATIVE: Native = Native(CodecName::Messages);
