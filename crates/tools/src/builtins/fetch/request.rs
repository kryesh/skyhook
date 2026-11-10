//! Request bodies as the target sends them.
use super::RequestBody;
use crate::error::AdmissionError;
use skyhook_worker::diagnostic::{Operation, Subject};
use skyhook_worker::{op, op::ContentType};

/// The body a request sends: its bytes, encoded as it is admitted, or a file on the
/// target.
pub(super) fn encode(body: RequestBody) -> Result<op::FetchBody, AdmissionError> {
    let prepare = |error: AdmissionError| {
        error.operation(
            Operation::Prepare,
            Subject::Label("HTTP request body".into()),
        )
    };
    let (bytes, content_type) = match body {
        RequestBody::Text { value } => (value.into_bytes(), Some(ContentType::Text)),
        RequestBody::Json { value } => (
            serde_json::to_vec(&value)
                .map_err(|error| prepare(AdmissionError::invalid_arguments(error)))?,
            Some(ContentType::Json),
        ),
        RequestBody::Form { fields } => {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.extend_pairs(fields);
            (form.finish().into_bytes(), Some(ContentType::Form))
        }
        RequestBody::Base64 { value } => {
            let bytes = skyhook_media::decode_base64_bounded(&value, usize::MAX)
                .map_err(|_| AdmissionError::invalid_arguments("invalid base64 body"));
            (bytes.map_err(prepare)?, None)
        }
        RequestBody::File { path } => return Ok(op::FetchBody::File { path }),
    };
    Ok(op::FetchBody::Bytes {
        content_type,
        length: bytes.len() as u64,
        input: op::Input::new(std::io::Cursor::new(bytes)),
    })
}
