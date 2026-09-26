//! OpenAI Chat Completions and the compatible servers that speak it.

mod decoder;
mod dialect;
mod encoder;
mod errors;
pub(crate) mod schema;
mod wire;

pub(crate) use decoder::Decoder;
pub(crate) use dialect::{Cache, Dialect, EmptyContent, ReasoningReplay, SystemRole, UsageRequest};
pub use dialect::{DataCollection, ProviderPreferences, Routing};
pub(crate) use encoder::{ASSISTANT_FIELDS, BODY_FIELDS, encode};
pub(crate) use errors::read as read_error;
