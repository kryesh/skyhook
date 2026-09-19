//! Allocation-bounded byte consumption. Callers supply their own semantic size policy.
//!
//! Metadata is an optional early filter, never a substitute for this consumption bound.
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt as _};

#[derive(Debug, thiserror::Error)]
pub enum BoundedReadError {
    #[error("input exceeds the {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Read through EOF, consuming at most `limit + 1` bytes. The extra byte detects
/// overflow even if the source grows after a metadata check. No partial payload
/// is returned on overflow or IO failure. The caller retains the reader.
pub async fn read_bounded<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Vec<u8>, BoundedReadError> {
    let mut bytes = Vec::new();
    let budget = (limit as u64).saturating_add(1);
    reader.take(budget).read_to_end(&mut bytes).await?;
    if bytes.len() > limit {
        return Err(BoundedReadError::TooLarge { limit });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exact_input_fits_and_overflow_consumes_one_byte_past_the_limit() {
        for (limit, input) in [(0, ""), (3, "abc"), (usize::MAX, "a")] {
            let bytes = read_bounded(&mut input.as_bytes(), limit).await.unwrap();
            assert_eq!(bytes, input.as_bytes());
        }
        for limit in [0, 3] {
            let mut endless = tokio::io::repeat(b'x').take(100);
            let error = read_bounded(&mut endless, limit).await.unwrap_err();
            assert!(
                matches!(error, BoundedReadError::TooLarge { limit: reported } if reported == limit)
            );
            assert_eq!(endless.limit(), 99 - limit as u64);
        }
    }
}
