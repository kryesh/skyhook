//! Streaming access to saved fields, which pages read without hydrating them.
use super::*;
use super::{elements::Container, json::saved_json};
use std::{collections::VecDeque, io::Cursor};
use struson::reader::{JsonReader, JsonStreamReader, ValueType};

/// How much of each stored string a reading needs.
#[derive(Clone, Copy)]
pub(super) enum Reading<'a> {
    /// Every byte, for values read whole.
    Whole,
    /// Enough of each stored string to show or sample it: its prefix, with its
    /// whole extent from storage rather than from scanning the rest. Strings in
    /// `complete` fields are read whole.
    Sampled {
        complete: &'a BTreeSet<FieldPointer>,
    },
}

impl Reading<'_> {
    /// Sampling what no complete field protects.
    pub(super) const SAMPLED: Reading<'static> = Reading::Sampled {
        complete: &BTreeSet::new(),
    };

    fn clips(self, field: &FieldPointer) -> bool {
        match self {
            Self::Whole => false,
            Self::Sampled { complete } => !complete
                .iter()
                .any(|complete| complete == field || complete.contains(field)),
        }
    }
}

/// Compact JSON text of the saved value at `field`, splicing stored fields in as
/// they are read, so memory holds only the compact document's inline parts.
pub(super) fn json_text(
    saved: &Saved,
    field: &FieldPointer,
    value: &Value,
    reading: Reading<'_>,
) -> Result<Spliced, ToolError> {
    let mut spliced = Spliced {
        parts: VecDeque::new(),
        inline: Vec::new(),
        clipped: BTreeMap::new(),
    };
    spliced.push(saved, field, value, reading)?;
    spliced.flush();
    Ok(spliced)
}

/// Extents of stored strings a sampled reading spliced as a prefix.
pub(super) type Clipped = BTreeMap<FieldPointer, crate::session::CaptureExtent>;

pub(super) struct Spliced {
    parts: VecDeque<Part>,
    inline: Vec<u8>,
    clipped: Clipped,
}

enum Part {
    Inline(Cursor<Vec<u8>>),
    Json(Source),
    Text(EscapedText),
}

impl Spliced {
    /// The stored strings spliced as a prefix, with their whole extents.
    pub(super) fn take_clipped(&mut self) -> Clipped {
        std::mem::take(&mut self.clipped)
    }

    fn push(
        &mut self,
        saved: &Saved,
        field: &FieldPointer,
        value: &Value,
        reading: Reading<'_>,
    ) -> Result<(), ToolError> {
        if let Some(source) = saved.stored(field)? {
            self.flush();
            let part = match (value.is_string(), reading.clips(field)) {
                (false, _) => Part::Json(source),
                (true, false) => Part::Text(EscapedText::new(Box::new(source))),
                (true, true) => {
                    let capture = saved
                        .captures
                        .get(field)
                        .ok_or_else(|| ToolError::failed("saved output field is missing"))?;
                    let extent = (saved.output.db.capture_extent(capture.id)).map_err(database)?;
                    self.clipped.insert(field.clone(), extent);
                    let mut prefix = Vec::new();
                    // One byte more, so that a cut sees what follows it.
                    let limit = preview::STRING_BYTES as u64 + 1;
                    source.take(limit).read_to_end(&mut prefix)?;
                    // Cut at a character boundary, so the prefix is still text.
                    let whole = std::str::from_utf8(&prefix)
                        .map_or_else(|error| error.valid_up_to(), str::len);
                    prefix.truncate(whole);
                    Part::Text(EscapedText::new(Box::new(Cursor::new(prefix))))
                }
            };
            self.parts.push_back(part);
            return Ok(());
        }
        if !saved.fields.iter().any(|stored| field.contains(stored)) {
            serde_json::to_writer(&mut self.inline, value)?;
            return Ok(());
        }
        match value {
            Value::Object(map) => {
                self.inline.push(b'{');
                for (index, (key, value)) in map.iter().enumerate() {
                    if index > 0 {
                        self.inline.push(b',');
                    }
                    serde_json::to_writer(&mut self.inline, key)?;
                    self.inline.push(b':');
                    self.push(saved, &field.property(key), value, reading)?;
                }
                self.inline.push(b'}');
            }
            Value::Array(items) => {
                self.inline.push(b'[');
                for (index, value) in items.iter().enumerate() {
                    if index > 0 {
                        self.inline.push(b',');
                    }
                    self.push(saved, &field.index(index), value, reading)?;
                }
                self.inline.push(b']');
            }
            scalar => serde_json::to_writer(&mut self.inline, scalar)?,
        }
        Ok(())
    }

    fn flush(&mut self) {
        if !self.inline.is_empty() {
            let inline = std::mem::take(&mut self.inline);
            self.parts.push_back(Part::Inline(Cursor::new(inline)));
        }
    }
}

impl Read for Spliced {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        while let Some(part) = self.parts.front_mut() {
            let count = match part {
                Part::Inline(cursor) => cursor.read(buffer)?,
                Part::Json(source) => source.read(buffer)?,
                Part::Text(text) => text.read(buffer)?,
            };
            if count > 0 {
                return Ok(count);
            }
            self.parts.pop_front();
        }
        Ok(0)
    }
}

/// A stored string's bytes as a JSON string, escaped as they are read.
struct EscapedText {
    input: BufReader<Box<dyn Read>>,
    pending: Vec<u8>,
    position: usize,
    stage: TextStage,
}

enum TextStage {
    Open,
    Body,
    Closed,
}

impl EscapedText {
    fn new(source: Box<dyn Read>) -> Self {
        Self {
            input: BufReader::with_capacity(IO_BUFFER_BYTES, source),
            pending: Vec::new(),
            position: 0,
            stage: TextStage::Open,
        }
    }
}

impl Read for EscapedText {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        while self.position == self.pending.len() {
            self.pending.clear();
            self.position = 0;
            match self.stage {
                TextStage::Open => {
                    self.pending.push(b'"');
                    self.stage = TextStage::Body;
                }
                TextStage::Body => {
                    let bytes = self.input.fill_buf()?;
                    if bytes.is_empty() {
                        self.pending.push(b'"');
                        self.stage = TextStage::Closed;
                    } else {
                        escape(bytes, &mut self.pending);
                        let count = bytes.len();
                        self.input.consume(count);
                    }
                }
                TextStage::Closed => return Ok(0),
            }
        }
        let count = buffer.len().min(self.pending.len() - self.position);
        buffer[..count].copy_from_slice(&self.pending[self.position..][..count]);
        self.position += count;
        Ok(count)
    }
}

/// JSON string escapes for saved UTF-8 text; other bytes pass through.
fn escape(bytes: &[u8], out: &mut Vec<u8>) {
    let mut run = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        if !matches!(byte, b'"' | b'\\' | 0..=31) {
            continue;
        }
        out.extend_from_slice(&bytes[run..index]);
        match byte {
            b'"' | b'\\' => out.extend_from_slice(&[b'\\', byte]),
            _ => out.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
        }
        run = index + 1;
    }
    out.extend_from_slice(&bytes[run..]);
}

/// How a selected field is paged.
pub(super) enum Resolved {
    /// Text read by lines: a string, a scalar's JSON, or a capture that is not a
    /// finished JSON field.
    Text(TextField),
    /// A reader positioned at an object or array.
    Json {
        json: Box<JsonField>,
        container: Container,
    },
}

/// A text field's bytes.
pub(super) enum TextField {
    Bytes(Source),
    /// A reader positioned at a string inside a stored container, copied only
    /// when it is paged.
    Stored(Box<JsonField>),
}

impl Resolved {
    /// The JSON value `json` is positioned at, which must be a container.
    pub(super) fn json(mut json: Box<JsonField>) -> Result<Self, ToolError> {
        let container = match json.reader.peek().map_err(saved_json)? {
            ValueType::Object => Container::Object,
            ValueType::Array => Container::Array,
            _ => return Err(saved_json("a stored field is not an object or array")),
        };
        Ok(Self::Json { json, container })
    }
}

impl TextField {
    /// The field's bytes: a string inside a stored container is copied once to
    /// disk, so text pages can seek within it.
    pub(super) fn source(
        self,
        cancellation: &crate::job::CancellationToken,
    ) -> Result<Source, ToolError> {
        let mut json = match self {
            Self::Bytes(source) => return Ok(source),
            Self::Stored(json) => json,
        };
        let mut file = tempfile::tempfile()?;
        let mut input = json.reader.next_string_reader().map_err(saved_json)?;
        let mut buffer = vec![0; IO_BUFFER_BYTES];
        loop {
            if cancellation.is_cancelled() {
                return Err(ToolError::cancelled());
            }
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            file.write_all(&buffer[..count])?;
        }
        std::io::Seek::rewind(&mut file)?;
        Ok(Source::Temporary(file))
    }
}

/// A reader positioned at a JSON value, and a meter that can bound what it reads
/// from there on.
pub(super) struct JsonField {
    pub(super) reader: JsonStreamReader<Metered>,
    pub(super) meter: Meter,
    /// Stored strings the reading spliced as a prefix, with their whole extents.
    pub(super) clipped: Clipped,
}

impl JsonField {
    pub(super) fn new(input: Box<dyn Read>, clipped: Clipped) -> Box<Self> {
        let meter = Meter::default();
        let reader = super::json::reader(Metered {
            input,
            meter: meter.clone(),
        });
        Box::new(Self {
            reader,
            meter,
            clipped,
        })
    }

    fn spliced(mut text: Spliced) -> Box<Self> {
        let clipped = text.take_clipped();
        Self::new(Box::new(text), clipped)
    }
}

/// The bytes a reader may still take from its source; unbounded until armed.
#[derive(Clone, Default)]
pub(super) struct Meter(std::rc::Rc<std::cell::Cell<Allowance>>);

#[derive(Clone, Copy, Default)]
enum Allowance {
    #[default]
    Unmetered,
    Left(u64),
    /// A read past the allowance was refused.
    Refused,
}

impl Meter {
    pub(super) fn arm(&self, bytes: u64) {
        self.0.set(Allowance::Left(bytes));
    }

    pub(super) fn refused(&self) -> bool {
        matches!(self.0.get(), Allowance::Refused)
    }
}

pub(super) struct Metered {
    input: Box<dyn Read>,
    meter: Meter,
}

impl Read for Metered {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let left = match self.meter.0.get() {
            Allowance::Unmetered => return self.input.read(buffer),
            Allowance::Left(left) if left > 0 || buffer.is_empty() => left,
            Allowance::Left(_) | Allowance::Refused => {
                self.meter.0.set(Allowance::Refused);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::FileTooLarge,
                    "input limit reached",
                ));
            }
        };
        let count = buffer
            .len()
            .min(usize::try_from(left).unwrap_or(usize::MAX));
        let read = self.input.read(&mut buffer[..count])?;
        self.meter.0.set(Allowance::Left(left - read as u64));
        Ok(read)
    }
}

/// The saved field at `field`; `None` when no output exists yet. A field inside
/// a stored container is reached by streaming, never by loading the container.
pub(super) fn resolve(
    saved: &Saved,
    field: &FieldPointer,
    reading: Reading<'_>,
    cancellation: &crate::job::CancellationToken,
) -> Result<Option<Resolved>, ToolError> {
    let document = saved.product.as_ref().map(|product| product.document());
    let value = document
        .as_ref()
        .and_then(|document| document.pointer(field.as_str()));
    if saved.fields.contains(field)
        && value.is_some_and(|value| value.is_object() || value.is_array())
    {
        let source = saved
            .stored(field)?
            .ok_or_else(|| ToolError::failed("saved output field is missing"))?;
        return Resolved::json(JsonField::new(Box::new(source), Clipped::new())).map(Some);
    }
    // Other captures, live or unfinished ones included, are their saved bytes.
    if let Some(source) = saved.capture(field) {
        return Ok(Some(Resolved::Text(TextField::Bytes(source))));
    }
    let Some(document) = &document else {
        return Ok(None);
    };
    if let Some(value) = value {
        return Ok(Some(match value {
            Value::String(text) => {
                Resolved::Text(TextField::Bytes(memory(text.as_bytes().to_vec())))
            }
            Value::Object(_) | Value::Array(_) => {
                Resolved::json(JsonField::spliced(json_text(saved, field, value, reading)?))?
            }
            scalar => Resolved::Text(TextField::Bytes(memory(serde_json::to_vec(scalar)?))),
        }));
    }
    let missing = || ToolError::invalid_arguments("field does not exist in this result");
    let ancestor = saved
        .fields
        .iter()
        .find(|stored| stored.contains(field))
        .ok_or_else(missing)?;
    let placeholder = document.pointer(ancestor.as_str()).ok_or_else(missing)?;
    let mut json = JsonField::spliced(json_text(saved, ancestor, placeholder, reading)?);
    let reader = &mut json.reader;
    let depth = ancestor.segments().count();
    for segment in field.segments().skip(depth) {
        if cancellation.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        if !descend(reader, &segment, cancellation)? {
            return Err(missing());
        }
    }
    Ok(Some(match reader.peek().map_err(saved_json)? {
        ValueType::Object | ValueType::Array => Resolved::json(json)?,
        ValueType::String => Resolved::Text(TextField::Stored(json)),
        ValueType::Number => {
            let token = reader.next_number_as_str().map_err(saved_json)?;
            Resolved::Text(TextField::Bytes(memory(token.as_bytes().to_vec())))
        }
        ValueType::Boolean => {
            let value = reader.next_bool().map_err(saved_json)?;
            Resolved::Text(TextField::Bytes(memory(value.to_string().into_bytes())))
        }
        ValueType::Null => {
            reader.next_null().map_err(saved_json)?;
            Resolved::Text(TextField::Bytes(memory(b"null".to_vec())))
        }
    }))
}

fn memory(bytes: Vec<u8>) -> Source {
    Source::Memory(Cursor::new(bytes))
}

/// Position `reader` at member or element `segment` of its next value; false
/// when that value has none.
fn descend<R: Read>(
    reader: &mut JsonStreamReader<R>,
    segment: &str,
    cancellation: &crate::job::CancellationToken,
) -> Result<bool, ToolError> {
    let cancelled = || {
        cancellation
            .is_cancelled()
            .then(ToolError::cancelled)
            .map_or(Ok(()), Err)
    };
    match reader.peek().map_err(saved_json)? {
        ValueType::Object => {
            reader.begin_object().map_err(saved_json)?;
            while reader.has_next().map_err(saved_json)? {
                cancelled()?;
                if reader.next_name().map_err(saved_json)? == segment {
                    return Ok(true);
                }
                reader.skip_value().map_err(saved_json)?;
            }
            Ok(false)
        }
        ValueType::Array => {
            let Some(index) = FieldPointer::array_index(segment) else {
                return Ok(false);
            };
            reader.begin_array().map_err(saved_json)?;
            for _ in 0..index {
                cancelled()?;
                if !reader.has_next().map_err(saved_json)? {
                    return Ok(false);
                }
                reader.skip_value().map_err(saved_json)?;
            }
            reader.has_next().map_err(saved_json)
        }
        _ => Ok(false),
    }
}

/// A rendering of the saved value at `field`, cached in the session.
pub(super) fn disk_rendering(
    saved: &Saved,
    field: &FieldPointer,
    write: impl FnOnce(&mut dyn Write) -> Result<(), ToolError>,
) -> Result<Source, ToolError> {
    let db = &saved.output.db;
    if let Some(capture) = db
        .rendering(saved.output.job.get(), field)
        .map_err(database)?
    {
        return Ok(Source::Capture(reader::CaptureReader::new(
            db.clone(),
            capture,
        )));
    }
    // Cache reservations are optional: contention or an unavailable cache can
    // still use private disk storage, without buffering the whole output.
    if let Ok(pending) = PendingCapture::rendering(&saved.output, field) {
        let mut writer = pending.open();
        write(&mut writer)?;
        let capture = writer.finish()?.capture_id();
        return Ok(Source::Capture(reader::CaptureReader::new(
            db.clone(),
            capture,
        )));
    }
    let mut writer = std::io::BufWriter::new(tempfile::tempfile()?);
    write(&mut writer)?;
    let mut file = writer.into_inner().map_err(|error| error.into_error())?;
    std::io::Seek::rewind(&mut file)?;
    Ok(Source::Temporary(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// Source failures still fail the read and leave its cache reservation reusable.
    #[tokio::test]
    async fn rendering_source_errors_propagate_without_poisoning_the_cache() {
        let (_root, manager, id) = super::super::tests::fixture(Some(json!({}))).await;
        manager.drain_supervisors().await;
        let output = manager.output(id);
        let saved = Saved::load(&output).unwrap();
        let field = FieldPointer::result().property("sequence");
        let failed = disk_rendering(&saved, &field, |writer| {
            Ok(json::write_sequence(b"invalid JSON".as_slice(), writer)?)
        });
        assert!(failed.is_err());
        assert_eq!(output.db.rendering(id.get(), &field).unwrap(), None);
        let mut source = disk_rendering(&saved, &field, |writer| {
            Ok(json::write_sequence(b"[1]\n[2]".as_slice(), writer)?)
        })
        .unwrap();
        assert!(matches!(source, Source::Capture(_)));
        let value: Value = serde_json::from_reader(&mut source).unwrap();
        assert_eq!(value, json!([[1], [2]]));
        assert!(output.db.rendering(id.get(), &field).unwrap().is_some());
    }

    /// Closing forbids cache writes, not reading a field that has never been rendered.
    #[tokio::test]
    async fn closed_output_still_renders_without_writing_a_cache() {
        let (_root, manager, id) =
            super::super::tests::fixture(Some(json!({"text": "saved"}))).await;
        manager.drain_supervisors().await;
        let output = manager.output(id);
        let saved = Saved::load(&output).unwrap();
        manager.store().close().await.unwrap();
        let field = FieldPointer::result().property("text");
        let mut source = disk_rendering(&saved, &field, |writer| {
            writer.write_all(b"saved")?;
            Ok(())
        })
        .unwrap();
        assert!(matches!(source, Source::Temporary(_)));
        let mut text = String::new();
        source.read_to_string(&mut text).unwrap();
        assert_eq!(text, "saved");
        assert_eq!(output.db.rendering(id.get(), &field).unwrap(), None);
    }
}
