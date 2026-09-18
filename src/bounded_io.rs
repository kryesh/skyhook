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
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::ReadBuf;

    struct ShortGrowingReader {
        consumed: usize,
        end: usize,
    }
    impl AsyncRead for ShortGrowingReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.consumed < self.end && buf.remaining() > 0 {
                buf.put_slice(b"x");
                self.consumed += 1;
            }
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn zero_exact_and_overflow_are_consumption_bounded() {
        for (limit, length, expected, consumed) in [
            (0, 0, Ok(0), 0),
            (0, 1, Err("too large"), 1),
            (3, 3, Ok(3), 3),
            (3, 4, Err("too large"), 4),
            (3, usize::MAX, Err("too large"), 4),
            (usize::MAX, 1, Ok(1), 1),
        ] {
            let mut reader = ShortGrowingReader {
                consumed: 0,
                end: length,
            };
            let result = read_bounded(&mut reader, limit).await;
            match (result, expected) {
                (Ok(bytes), Ok(expected)) => assert_eq!(bytes.len(), expected),
                (Err(BoundedReadError::TooLarge { limit: reported }), Err("too large")) => {
                    assert_eq!(reported, limit);
                }
                (result, expected) => panic!("limit {limit}: {result:?} is not {expected:?}"),
            }
            assert_eq!(reader.consumed, consumed, "limit {limit} length {length}");
        }
    }
}
