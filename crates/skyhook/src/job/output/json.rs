//! Streaming reads of saved JSON bytes, with memory bounded by nesting depth and
//! the largest single key or number rather than by value size. `struson` panics
//! rather than erring on out-of-order calls, so every reader it builds is made
//! here, and callers follow its call order.
use std::{
    cell::RefCell,
    collections::HashSet,
    hash::{BuildHasher, RandomState},
    io::{self, BufRead, Read},
};

use serde_json::Value;
use struson::reader::{JsonReader, JsonStreamReader, ReaderSettings, ValueType};

use super::ToolError;
use crate::named_enum::named_enum;

/// Longer number tokens are treated as inexact without parsing them.
const NUMBER_TOKEN_BYTES: usize = 1024;
/// Deeper JSON stays text, leaving room below `serde_json`'s nesting limit for the
/// results, views and script values a detected value is read inside.
const DEPTH: u32 = 64;

named_enum! {
    /// What a finished text capture declared as JSON actually holds.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    pub enum Detection {
        /// One object.
        Object = "object",
        /// One array.
        Array = "array",
        /// Two or more whitespace-separated objects or arrays, read as one array.
        Sequence = "sequence",
        NotJson = "not_json",
        /// A number whose double does not print back as the same decimal value.
        InexactNumber = "inexact_number",
        DuplicateKey = "duplicate_key",
    }
}

impl Detection {
    /// The empty container a JSON field presents as in the compact document;
    /// none when the field stays text.
    pub(crate) fn placeholder(self) -> Option<Value> {
        match self {
            Self::Object => Some(Value::Object(Default::default())),
            Self::Array | Self::Sequence => Some(Value::Array(Vec::new())),
            Self::NotJson | Self::InexactNumber | Self::DuplicateKey => None,
        }
    }
}

/// Classify `input` in one pass. Storage failures are errors; malformed or
/// unsupported JSON is an outcome.
pub(crate) fn detect(input: impl Read) -> io::Result<Detection> {
    let failure = RefCell::new(None);
    let mut reader = detecting(input, &failure);
    let scanned = scan(&mut reader, &RandomState::new());
    drop(reader);
    // struson reports syntax errors inside strings as IO errors too, so only a
    // failure the input itself recorded is a storage failure.
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    Ok(match scanned {
        Ok((1, ValueType::Object)) => Detection::Object,
        Ok((1, _)) => Detection::Array,
        Ok(_) => Detection::Sequence,
        Err(Stop(detection)) => detection,
    })
}

/// The JSON value `text` holds under the detection rules, a sequence read as an
/// array; none when it stays text.
pub(crate) fn parse_text(text: &str) -> Option<Value> {
    match detect(text.as_bytes()).ok()? {
        Detection::Object | Detection::Array => serde_json::from_str(text).ok(),
        Detection::Sequence => serde_json::Deserializer::from_str(text)
            .into_iter::<Value>()
            .collect::<Result<Vec<_>, _>>()
            .ok()
            .map(Value::Array),
        Detection::NotJson | Detection::InexactNumber | Detection::DuplicateKey => None,
    }
}

/// A detected `Sequence` read as the array it presents, keeping every byte of its
/// values. Detection already validated the input as whitespace-separated objects
/// and arrays, so only string and nesting state decide where commas go.
pub(super) struct SequenceArray<R> {
    input: R,
    stage: Stage,
    scan: Scan,
    depth: u32,
    /// A top-level value ended, so the next one is preceded by a comma.
    separate: bool,
}

enum Stage {
    Open,
    Values,
    Closed,
}

/// What the previous byte left the scan inside.
#[derive(Clone, Copy)]
enum Scan {
    Structure,
    String,
    Escape,
}

impl<R: BufRead> SequenceArray<R> {
    pub(super) fn new(input: R) -> Self {
        Self {
            input,
            stage: Stage::Open,
            scan: Scan::Structure,
            depth: 0,
            separate: false,
        }
    }
}

impl<R: BufRead> Read for SequenceArray<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let Some(first) = buffer.first_mut() else {
            return Ok(0);
        };
        match self.stage {
            Stage::Open => {
                *first = b'[';
                self.stage = Stage::Values;
                return Ok(1);
            }
            Stage::Closed => return Ok(0),
            Stage::Values => {}
        }
        let bytes = self.input.fill_buf()?;
        if bytes.is_empty() {
            *first = b']';
            self.stage = Stage::Closed;
            return Ok(1);
        }
        let (mut read, mut written) = (0, 0);
        while read < bytes.len() && written < buffer.len() {
            let byte = bytes[read];
            match (self.scan, byte) {
                (Scan::Escape, _) => self.scan = Scan::String,
                (Scan::String, b'\\') => self.scan = Scan::Escape,
                (Scan::String, b'"') => self.scan = Scan::Structure,
                (Scan::String, _) => {}
                (Scan::Structure, b'{' | b'[') if self.depth == 0 && self.separate => {
                    buffer[written] = b',';
                    written += 1;
                    self.separate = false;
                    continue;
                }
                (Scan::Structure, b'"') => self.scan = Scan::String,
                (Scan::Structure, b'{' | b'[') => self.depth += 1,
                (Scan::Structure, b'}' | b']') => {
                    self.depth -= 1;
                    self.separate = self.depth == 0;
                }
                (Scan::Structure, _) => {}
            }
            buffer[written] = byte;
            (read, written) = (read + 1, written + 1);
        }
        self.input.consume(read);
        Ok(written)
    }
}

/// A streaming reader over one saved JSON value.
pub(super) fn reader<R: Read>(input: R) -> JsonStreamReader<R> {
    JsonStreamReader::new_custom(input, settings())
}

pub(super) fn saved_json(error: impl std::fmt::Display) -> ToolError {
    ToolError::failed(format!("saved output is not valid JSON: {error}"))
}

fn settings() -> ReaderSettings {
    ReaderSettings {
        track_path: false,
        // Its fixed exponent bound rejects exact numbers such as 1e100.
        restrict_number_values: false,
        ..ReaderSettings::default()
    }
}

/// A reader over captured text that may hold a sequence of values.
fn detecting<R: Read>(
    input: R,
    failure: &RefCell<Option<io::Error>>,
) -> JsonStreamReader<Tracked<'_, R>> {
    let settings = ReaderSettings {
        allow_multiple_top_level: true,
        max_nesting_depth: Some(DEPTH),
        ..settings()
    };
    JsonStreamReader::new_custom(Tracked { input, failure }, settings)
}

/// Records the input's own failure before struson wraps or replaces it.
struct Tracked<'a, R> {
    input: R,
    failure: &'a RefCell<Option<io::Error>>,
}

impl<R: Read> Read for Tracked<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.input.read(buffer).inspect_err(|error| {
            *self.failure.borrow_mut() = Some(io::Error::new(error.kind(), error.to_string()));
        })
    }
}

/// Ends a scan with its outcome. Any reader error ends it as `NotJson`; the
/// caller consults the recorded storage failure first.
struct Stop(Detection);

impl<E> From<E> for Stop
where
    E: std::error::Error,
{
    fn from(_: E) -> Self {
        Self(Detection::NotJson)
    }
}

/// The number of top-level values, each an object or array, and the first one's type.
fn scan<R: Read>(
    reader: &mut JsonStreamReader<R>,
    keys: &RandomState,
) -> Result<(usize, ValueType), Stop> {
    let first = reader.peek()?;
    let mut values = 0;
    // An empty document is invalid, so `has_next` is defined only after a value.
    while values == 0 || reader.has_next()? {
        if !matches!(reader.peek()?, ValueType::Object | ValueType::Array) {
            return Err(Stop(Detection::NotJson));
        }
        visit(reader, keys)?;
        values += 1;
    }
    Ok((values, first))
}

fn visit<R: Read>(reader: &mut JsonStreamReader<R>, keys: &RandomState) -> Result<(), Stop> {
    match reader.peek()? {
        ValueType::Object => {
            reader.begin_object()?;
            let mut seen = HashSet::new();
            while reader.has_next()? {
                // A 64-bit collision only keeps the field as text.
                if !seen.insert(keys.hash_one(reader.next_name()?)) {
                    return Err(Stop(Detection::DuplicateKey));
                }
                visit(reader, keys)?;
            }
            reader.end_object()?;
        }
        ValueType::Array => {
            reader.begin_array()?;
            while reader.has_next()? {
                visit(reader, keys)?;
            }
            reader.end_array()?;
        }
        ValueType::Number => {
            if !exact(reader.next_number_as_str()?) {
                return Err(Stop(Detection::InexactNumber));
            }
        }
        ValueType::String => {
            io::copy(&mut reader.next_string_reader()?, &mut io::sink())?;
        }
        ValueType::Boolean | ValueType::Null => reader.skip_value()?,
    }
    Ok(())
}

/// Whether a JSON number token's nearest double prints back as the same decimal
/// value, so Rust, scripts and double-precision comparisons all see the written number.
pub(crate) fn exact(token: &str) -> bool {
    if token.len() > NUMBER_TOKEN_BYTES {
        return false;
    }
    let Ok(double) = token.parse::<f64>() else {
        return false;
    };
    // Both print the shortest decimal that round-trips, but may break a tie between
    // two such decimals differently: admit only what every reader prints as written.
    let written = decimal(token);
    let rust = format!("{double:e}");
    let json = serde_json::Number::from_f64(double).map(|number| number.to_string());
    json.is_some_and(|json| written == decimal(&json)) && written == decimal(&rust)
}

/// A number's value as (negative, significant digits, power of ten), with every
/// zero spelled the same.
fn decimal(number: &str) -> (bool, String, i64) {
    let (negative, unsigned) = number
        .strip_prefix('-')
        .map_or((false, number), |rest| (true, rest));
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        // Nonzero digits with an out-of-range exponent never compare equal: their
        // double is infinite or zero, which does not print them back.
        .map_or((unsigned, 0), |(mantissa, exponent)| {
            (mantissa, exponent.parse::<i64>().unwrap_or(i64::MAX))
        });
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{integer}{fraction}");
    let leading = digits.trim_start_matches('0');
    let significant = leading.trim_end_matches('0');
    if significant.is_empty() {
        return (false, String::new(), 0);
    }
    let scale = exponent
        .saturating_sub(fraction.len() as i64)
        .saturating_add((leading.len() - significant.len()) as i64);
    (negative, significant.to_owned(), scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_exact_only_when_their_double_prints_them_back() {
        for token in [
            "0.1",
            "1.0",
            "1e30",
            "1E+30",
            "1e100",
            "9007199254740992",
            "-0.0",
            "5e-324",
            "0e99999999999999999999",
        ] {
            assert!(exact(token), "{token}");
        }
        let long = format!("1.{}", "0".repeat(NUMBER_TOKEN_BYTES));
        for token in [
            "9007199254740993",
            "12345678901234567890",
            "0.10000000000000000001",
            "9007199254740993.0",
            // Equally short decimals of one double, which printers choose between.
            "1000000000000000.2",
            "1000000000000000.3",
            "1e400",
            "1e-400",
            long.as_str(),
        ] {
            assert!(!exact(token), "{token}");
        }
    }

    #[test]
    fn detection_accepts_one_document_or_a_whitespace_separated_sequence_of_containers() {
        let deep = format!("{}{}", "[".repeat(65), "]".repeat(65));
        for (text, expected) in [
            (r#"  {"a":[1,2,{"b":null,"c":"é\n"}]}  "#, Detection::Object),
            ("{\"a\":1}\n{\"a\":2}\n\n[3]\n", Detection::Sequence),
            ("{\n  \"a\": 1\n}\n[\n  2\n]", Detection::Sequence),
            (r#"[{"a":1},{"a":2}]"#, Detection::Array),
            ("42\n", Detection::NotJson),
            ("{\"a\":1}\n42\n", Detection::NotJson),
            ("{\"a\":1} done", Detection::NotJson),
            ("{\"a\":[1,2", Detection::NotJson),
            (r#"{"a":"\q"}"#, Detection::NotJson),
            ("", Detection::NotJson),
            (&deep, Detection::NotJson),
            (r#"{"a":1,"b":2,"a":3}"#, Detection::DuplicateKey),
            (r#"{"id":9007199254740993}"#, Detection::InexactNumber),
        ] {
            assert_eq!(detect(text.as_bytes()).unwrap(), expected, "{text}");
        }
    }

    /// Brackets and quotes inside strings are text, and a one-byte buffer still
    /// sees every comma.
    #[test]
    fn sequences_read_as_the_array_they_present() {
        let text = "{\"s\": \"}{\\\"[\\\\\"}\n\n[1.50,\n [2]]{}\n";
        let expected = "[{\"s\": \"}{\\\"[\\\\\"}\n\n,[1.50,\n [2]],{}\n]";
        let mut whole = String::new();
        SequenceArray::new(text.as_bytes())
            .read_to_string(&mut whole)
            .unwrap();
        assert_eq!(whole, expected);
        let mut bytewise = Vec::new();
        let mut byte = [0];
        let mut array = SequenceArray::new(text.as_bytes());
        while array.read(&mut byte).unwrap() == 1 {
            bytewise.push(byte[0]);
        }
        assert_eq!(bytewise, expected.as_bytes());
        let value: Value = serde_json::from_str(&whole).unwrap();
        assert_eq!(
            value,
            serde_json::json!([{"s": "}{\"[\\"}, [1.50, [2]], {}])
        );
    }

    #[test]
    fn storage_failures_are_errors_rather_than_outcomes() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("capture chunk missing"))
            }
        }
        let error = detect(Failing).unwrap_err();
        assert_eq!(error.to_string(), "capture chunk missing");
    }
}
