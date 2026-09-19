//! Native OpenAI Chat Completions with compatible visible reasoning deltas.

mod decoder;
mod encoder;
mod schema;
mod wire;

pub(crate) use decoder::Decoder;
pub(crate) use encoder::encode;
#[cfg(test)]
pub(crate) use schema::validate_schema;

pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
