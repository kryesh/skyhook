//! WinRM's MIME envelope for a sealed message: the original length, then the
//! signature's length, the signature, and the sealed bytes.

use super::error::DecodeError;

/// The content type of a sealed message, as WinRM's HTTP listener expects it.
pub(super) const ENCRYPTED: &str = "multipart/encrypted;protocol=\"application/HTTP-SPNEGO-session-encrypted\";boundary=\"Encrypted Boundary\"";
pub(super) const SOAP: &str = "application/soap+xml;charset=UTF-8";
const BOUNDARY: &[u8] = b"--Encrypted Boundary\r\n";
const LAST_BOUNDARY: &[u8] = b"--Encrypted Boundary--\r\n";
const OCTET_STREAM: &[u8] = b"\tContent-Type: application/octet-stream\r\n";

/// A sealed message of `length` original bytes.
pub(super) struct Sealed<'a> {
    pub length: usize,
    pub signature: &'a [u8],
    pub sealed: &'a [u8],
}

impl<'a> Sealed<'a> {
    pub(super) fn frame(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(self.sealed.len() + 256);
        body.extend_from_slice(BOUNDARY);
        body.extend_from_slice(b"\tContent-Type: application/HTTP-SPNEGO-session-encrypted\r\n");
        body.extend_from_slice(
            format!("\tOriginalContent: type={SOAP};Length={}\r\n", self.length).as_bytes(),
        );
        body.extend_from_slice(BOUNDARY);
        body.extend_from_slice(OCTET_STREAM);
        let size = u32::try_from(self.signature.len()).expect("a signature is a few bytes");
        body.extend_from_slice(&size.to_le_bytes());
        body.extend_from_slice(self.signature);
        body.extend_from_slice(self.sealed);
        body.extend_from_slice(LAST_BOUNDARY);
        body
    }

    pub(super) fn unframe(body: &'a [u8]) -> Result<Self, DecodeError> {
        let body = body.strip_prefix(BOUNDARY).ok_or(DecodeError::Framing)?;
        // The header is text, so the boundary's next occurrence ends it; the sealed
        // bytes may hold any bytes and run up to the closing boundary.
        let end = (body.windows(BOUNDARY.len()))
            .position(|window| window == BOUNDARY)
            .ok_or(DecodeError::Framing)?;
        let (header, payload) = (&body[..end], &body[end + BOUNDARY.len()..]);
        let length = (std::str::from_utf8(header).ok())
            .and_then(|header| header.split("Length=").nth(1))
            .and_then(|length| length.trim().parse().ok())
            .ok_or(DecodeError::Framing)?;
        let payload = (payload.strip_prefix(OCTET_STREAM))
            .and_then(|payload| payload.strip_suffix(LAST_BOUNDARY))
            .ok_or(DecodeError::Framing)?;
        let (size, rest) = payload
            .split_first_chunk::<4>()
            .ok_or(DecodeError::Framing)?;
        let size = usize::try_from(u32::from_le_bytes(*size)).map_err(|_| DecodeError::Framing)?;
        let (signature, sealed) = rest.split_at_checked(size).ok_or(DecodeError::Framing)?;
        Ok(Self {
            length,
            signature,
            sealed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A framed message unframes to its length, signature and sealed bytes, even when
    /// the sealed bytes hold the boundaries themselves; a cut body is malformed.
    #[test]
    fn frames_round_trip_and_cut_ones_are_malformed() {
        let sealed = [b"\x00\xff".as_slice(), BOUNDARY, LAST_BOUNDARY, b"\r\n"].concat();
        let message = Sealed {
            length: 42,
            signature: &[7; 16],
            sealed: &sealed,
        };
        let body = message.frame();
        let read = Sealed::unframe(&body).unwrap();
        assert_eq!(
            (read.length, read.signature, read.sealed),
            (42, &[7; 16][..], &sealed[..])
        );
        let cut = &body[..body.len() - 1];
        assert!(matches!(Sealed::unframe(cut), Err(DecodeError::Framing)));
        assert!(matches!(Sealed::unframe(b""), Err(DecodeError::Framing)));
    }
}
