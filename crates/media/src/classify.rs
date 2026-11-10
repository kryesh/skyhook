//! The one rule for what bytes are: a supported image's signature means an image, which
//! must decode; a NUL byte or invalid UTF-8 means binary, even in valid UTF-8 for a NUL;
//! the rest is text.

use super::{DecodeError, Image, ImageFormat, MAX_IMAGE_BYTES};

/// Classify `bytes` whole, which become the image or the text they are.
pub async fn classify(bytes: Vec<u8>) -> Result<Finished, DecodeError> {
    if ImageFormat::sniff(&bytes).is_some() {
        return Image::decode(bytes).await.map(Finished::Image);
    }
    Ok(match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Finished::Text(text),
        Ok(_) | Err(_) => Finished::Binary,
    })
}

/// Bytes classified as they arrive. Text is handed back as it completes; an image's bytes
/// are kept, within [`MAX_IMAGE_BYTES`], until it decodes at the end.
pub struct Classifier(State);

impl Default for Classifier {
    fn default() -> Self {
        Self(State::Opening(Vec::new()))
    }
}

enum State {
    /// Fewer bytes than an image signature takes.
    Opening(Vec<u8>),
    /// Text so far, and a sequence the last chunk split.
    Text(Vec<u8>),
    Image(Vec<u8>),
    Binary,
}

/// What a chunk showed of the bytes so far.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Undecided, or an image whose bytes are kept.
    Pending,
    /// Text: what the chunk completed of it.
    Text(String),
    Binary,
}

/// What the bytes were, once they end.
#[derive(Debug, PartialEq, Eq)]
pub enum Finished {
    /// Text: what no chunk handed back of it.
    Text(String),
    Image(Image),
    Binary,
}

impl Classifier {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Step, DecodeError> {
        match &mut self.0 {
            State::Opening(opening) => {
                opening.extend_from_slice(chunk);
                if opening.len() < ImageFormat::SIGNATURE_BYTES {
                    return Ok(Step::Pending);
                }
                let opening = std::mem::take(opening);
                self.open(opening)
            }
            State::Text(pending) => {
                pending.extend_from_slice(chunk);
                let pending = std::mem::take(pending);
                Ok(self.text(pending))
            }
            State::Image(image) => {
                if (image.len() + chunk.len()) as u64 > MAX_IMAGE_BYTES {
                    return Err(DecodeError::TooLarge);
                }
                image.extend_from_slice(chunk);
                Ok(Step::Pending)
            }
            State::Binary => Ok(Step::Binary),
        }
    }

    pub async fn finish(mut self) -> Result<Finished, DecodeError> {
        if let State::Opening(opening) = &mut self.0 {
            let opening = std::mem::take(opening);
            if let Step::Text(text) = self.open(opening)? {
                return Ok(match self.0 {
                    State::Text(pending) if !pending.is_empty() => Finished::Binary,
                    _ => Finished::Text(text),
                });
            }
        }
        match self.0 {
            // A sequence still split at the end is not text.
            State::Text(pending) if pending.is_empty() => Ok(Finished::Text(String::new())),
            State::Image(image) => Image::decode(image).await.map(Finished::Image),
            State::Opening(_) | State::Text(_) | State::Binary => Ok(Finished::Binary),
        }
    }

    /// Decide on the opening bytes: an image by their signature, else text or binary.
    fn open(&mut self, opening: Vec<u8>) -> Result<Step, DecodeError> {
        if ImageFormat::sniff(&opening).is_none() {
            return Ok(self.text(opening));
        }
        if opening.len() as u64 > MAX_IMAGE_BYTES {
            return Err(DecodeError::TooLarge);
        }
        self.0 = State::Image(opening);
        Ok(Step::Pending)
    }

    /// The text `pending` completes, keeping a sequence it splits, or binary.
    fn text(&mut self, mut pending: Vec<u8>) -> Step {
        let valid = match std::str::from_utf8(&pending) {
            _ if pending.contains(&0) => None,
            Ok(text) => Some(text.len()),
            Err(error) => error.error_len().is_none().then(|| error.valid_up_to()),
        };
        let Some(valid) = valid else {
            self.0 = State::Binary;
            return Step::Binary;
        };
        self.0 = State::Text(pending.split_off(valid));
        Step::Text(String::from_utf8(pending).expect("a validated UTF-8 prefix"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[tokio::test]
    async fn classification_decodes_images_and_treats_nul_as_binary() {
        let png = crate::tests::png(b"\0").await;
        let classified = classify(png.bytes().to_vec()).await.unwrap();
        assert_eq!(classified, Finished::Image(png));
        assert_matches!(
            classify(b"\x89PNG\r\n\x1a\ntext".to_vec()).await,
            Err(DecodeError::Invalid(_))
        );
        for text in ["", "text"] {
            assert_matches!(
                classify(text.as_bytes().to_vec()).await.unwrap(),
                Finished::Text(classified) if classified == text
            );
        }
        for binary in [&b"text\0"[..], b"\xff", b"split \xe2\x82"] {
            assert_eq!(classify(binary.to_vec()).await.unwrap(), Finished::Binary);
        }
    }

    /// Chunks decide as the whole does: text is handed back as each completes it, a
    /// signature split across chunks still names an image, and an image is bounded.
    #[tokio::test]
    async fn chunks_classify_as_their_whole() {
        let mut classifier = Classifier::default();
        let mut steps = Vec::new();
        for chunk in [&b"text \xe2"[..], b"\x82\xac and more", b" text"] {
            steps.push(classifier.push(chunk).unwrap());
        }
        assert_eq!(
            steps,
            [
                Step::Pending,
                Step::Text("text \u{20ac} and more".into()),
                Step::Text(" text".into())
            ]
        );
        assert_matches!(classifier.finish().await, Ok(Finished::Text(rest)) if rest.is_empty());
        let mut classifier = Classifier::default();
        assert_eq!(
            classifier.push(b"long enough text").unwrap(),
            Step::Text("long enough text".into())
        );
        assert_eq!(classifier.push(b"\0").unwrap(), Step::Binary);
        let png = crate::tests::png(b"").await;
        let mut classifier = Classifier::default();
        for chunk in png.bytes().chunks(5) {
            assert_eq!(classifier.push(chunk).unwrap(), Step::Pending);
        }
        assert_matches!(classifier.finish().await, Ok(Finished::Image(image)) if image == png);
        let mut classifier = Classifier::default();
        classifier.push(png.bytes()).unwrap();
        let rest = vec![0; MAX_IMAGE_BYTES as usize];
        assert_matches!(classifier.push(&rest), Err(DecodeError::TooLarge));
    }
}
