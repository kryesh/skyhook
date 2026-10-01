//! One streaming pass over a value: its exhaustive shape and a pool of what
//! fitting may show of it, within bounded memory.
//!
//! Candidates are pooled by rank: the smallest shared sample count that shows a
//! candidate, less one. A candidate is pooled while the bytes pooled at its rank
//! and below, and for first samples at its depth and shallower, are within the
//! pool, so neither earlier containers' later samples nor their deeper detail
//! crowds out a later container's first ones. Candidates beyond the samples are
//! pooled only while a fit showing everything could still be within the budget.
//! Records keep every member: past the pool, a container member is emptied.
use std::io::Read;

use serde_json::Value;
use struson::{
    reader::{JsonReader, JsonStreamReader, ValueType},
    writer::{JsonStreamWriter, JsonWriter},
};

use super::{
    Accounting, Pooled, SAMPLE_TEXT_BYTES, SAMPLES, TEXT_ALLOWANCE, TEXT_BYTES, json_bytes,
    protects,
};
use crate::job::output::{
    CONTENT_BYTES, FieldPointer, ToolError,
    json::saved_json,
    projection::Presented,
    render::Clipped,
    shape::{COLLECTION_MEMBERS, ObjectBuilder, Scalar, Shape},
};

/// Bytes pooled that a candidate competes with before it is skipped.
const POOL_BYTES: usize = 4 * CONTENT_BYTES;

/// Pool the reader's next value, the value at `field`.
pub(in crate::job::output) fn pool<R: Read>(
    reader: &mut JsonStreamReader<R>,
    field: &FieldPointer,
    presented: &Presented,
    clipped: &Clipped,
    cancellation: &crate::job::CancellationToken,
    accounting: Accounting,
) -> Result<Pooled, ToolError> {
    let mut walker = Walker {
        reader,
        presented,
        clipped,
        cancellation,
        accounting,
        tally: Tally::default(),
        memory: 0,
        omitted: false,
        text_bytes: 0,
    };
    let (shape, node) = walker.walk(field, true, Place::ROOT)?;
    Ok(Pooled::new(shape, node, !walker.omitted, accounting))
}

/// A pooled value: what fitting may show of it.
pub(super) enum Node {
    Value(Value),
    Complete(Value),
    Text(Text),
    Array {
        /// Pooled elements with their source indices.
        items: Vec<(usize, Node)>,
        total: usize,
    },
    Object {
        /// Pooled members with their source indices, in source order.
        members: Vec<(usize, String, Node)>,
        total: usize,
    },
}

impl Node {
    /// A whole in-memory value at `field` for `accounting`, its presented fields
    /// marked.
    pub(super) fn of(
        value: Value,
        field: &FieldPointer,
        presented: &Presented,
        accounting: Accounting,
    ) -> Self {
        let mut marker = Marker {
            presented,
            accounting,
            text_bytes: 0,
        };
        // Pointers are only needed to find presented fields.
        marker.mark(value, (!presented.is_empty()).then(|| field.clone()), true)
    }
}

/// How an in-memory value's nodes are classified, as a streamed one's are.
struct Marker<'a> {
    presented: &'a Presented,
    accounting: Accounting,
    /// Bytes of text fields so far, within `TEXT_ALLOWANCE`.
    text_bytes: usize,
}

impl Marker<'_> {
    fn mark(&mut self, value: Value, field: Option<FieldPointer>, record: bool) -> Node {
        if field
            .as_ref()
            .is_some_and(|field| self.presented.complete.contains(field))
        {
            return Node::Complete(value);
        }
        let child = |make: &dyn Fn(&FieldPointer) -> FieldPointer| field.as_ref().map(make);
        match value {
            Value::String(text) => {
                let keep = keep(self.presented, field.as_ref());
                let mut text = Text {
                    limit: text.len(),
                    bytes: text.len(),
                    newlines: text.bytes().filter(|&byte| byte == b'\n').count(),
                    ends_line: text.ends_with('\n'),
                    end: Vec::new(),
                    prefix: text.into_bytes(),
                    role: TextRole::Sample,
                };
                text.role = role(&mut self.text_bytes, self.accounting, record, keep, &text);
                if text.role == TextRole::Field(Keep::Ends) {
                    text.end = text.prefix[text.bytes.saturating_sub(TEXT_BYTES)..].to_vec();
                }
                Node::Text(text)
            }
            Value::Array(items) => Node::Array {
                total: items.len(),
                items: (items.into_iter().enumerate())
                    .map(|(index, item)| {
                        let field = child(&|field| field.index(index));
                        (index, self.mark(item, field, false))
                    })
                    .collect(),
            },
            Value::Object(map) => {
                let record = record && map.len() <= COLLECTION_MEMBERS;
                Node::Object {
                    total: map.len(),
                    members: (map.into_iter().enumerate())
                        .map(|(index, (key, value))| {
                            let field = child(&|field| field.property(&key));
                            (index, key, self.mark(value, field, record))
                        })
                        .collect(),
                }
            }
            scalar => Node::Value(scalar),
        }
    }
}

/// Whether a string, reached only through record members when `record`, is a
/// text field: previews give those their own limits outside the budget until
/// they have used `TEXT_ALLOWANCE`, charging each its most shown bytes.
fn role(
    text_bytes: &mut usize,
    accounting: Accounting,
    record: bool,
    keep: Keep,
    text: &Text,
) -> TextRole {
    let charged = text.prefix.len().min(TEXT_BYTES);
    if record && accounting == Accounting::Preview && *text_bytes + charged <= TEXT_ALLOWANCE {
        *text_bytes += charged;
        TextRole::Field(keep)
    } else {
        TextRole::Sample
    }
}

/// Which lines the string at `field` keeps as a text field.
fn keep(presented: &Presented, field: Option<&FieldPointer>) -> Keep {
    if field.is_some_and(|field| presented.ends.contains(field)) {
        Keep::Ends
    } else {
        Keep::Head
    }
}

/// How a fit shortens a string.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TextRole {
    /// A text field: its own line and byte limits, outside the budget.
    Field(Keep),
    /// Within a sample, clipped as the fit allows and counted in the budget.
    Sample,
}

/// The lines a text field over its limits keeps.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Keep {
    /// Its first lines.
    Head,
    /// Its first and last lines, around a gap.
    Ends,
}

/// A string's leading bytes and the extent of the whole string.
pub(super) struct Text {
    pub(super) role: TextRole,
    /// Up to `limit` bytes, and one more when the string goes on, so that a cut
    /// sees what follows it.
    pub(super) prefix: Vec<u8>,
    /// The string's last bytes, up to `TEXT_BYTES`, when it is a text field
    /// keeping both ends.
    pub(super) end: Vec<u8>,
    /// The most bytes a fit keeping all that was pooled shows.
    pub(super) limit: usize,
    pub(super) bytes: usize,
    pub(super) newlines: usize,
    pub(super) ends_line: bool,
}

/// Where a candidate sits: its rank, the smallest shared sample count that
/// shows it less one, and its depth, the containers around it.
#[derive(Clone, Copy)]
struct Place {
    rank: usize,
    depth: usize,
}

impl Place {
    const ROOT: Self = Self { rank: 0, depth: 0 };

    /// Element or collection member `index` of the container here.
    fn child(self, index: usize) -> Self {
        Self {
            rank: self.rank.max(index),
            depth: self.depth + 1,
        }
    }

    /// A record member of the object here.
    fn member(self) -> Self {
        Self {
            rank: self.rank,
            depth: self.depth + 1,
        }
    }
}

/// Bytes pooled by where fits need them. Fits showing more than one sample show
/// every depth, and those limiting depth show one, so only the first samples
/// are counted by depth: a candidate among them competes only with what is
/// pooled at its depth and shallower, so detail within one never crowds out a
/// later container's first sample.
#[derive(Clone, Default)]
struct Tally {
    /// Rank 0, by depth.
    first: Vec<usize>,
    /// Ranks 1 to `SAMPLES`, the last counting candidates beyond the samples.
    later: [usize; SAMPLES],
}

impl Tally {
    fn add(&mut self, place: Place, bytes: usize) {
        match place.rank {
            0 => {
                if self.first.len() <= place.depth {
                    self.first.resize(place.depth + 1, 0);
                }
                self.first[place.depth] += bytes;
            }
            rank => self.later[rank.min(SAMPLES) - 1] += bytes,
        }
    }

    /// What a candidate at `place`, of rank below `SAMPLES`, competes with.
    fn before(&self, place: Place) -> usize {
        match place.rank {
            0 => self.first.iter().take(place.depth + 1).sum(),
            rank => self.first.iter().sum::<usize>() + self.later[..rank].iter().sum::<usize>(),
        }
    }

    fn total(&self) -> usize {
        self.first.iter().sum::<usize>() + self.later.iter().sum::<usize>()
    }

    /// What was counted since `earlier`, a copy of this tally.
    fn since(&self, earlier: &Self) -> Self {
        let before = |depth| earlier.first.get(depth).copied().unwrap_or(0);
        Self {
            first: (self.first.iter().enumerate())
                .map(|(depth, bytes)| bytes - before(depth))
                .collect(),
            later: std::array::from_fn(|rank| self.later[rank] - earlier.later[rank]),
        }
    }

    /// Move `counted`, what a collection's leading member at `index` added while
    /// taken as a record's, to the ranks its position gives it.
    fn rerank(&mut self, counted: &Self, index: usize) {
        if index == 0 {
            return;
        }
        for (depth, &bytes) in counted.first.iter().enumerate() {
            self.first[depth] -= bytes;
            self.later[index.min(SAMPLES) - 1] += bytes;
        }
        for (slot, &bytes) in counted.later.iter().enumerate() {
            self.later[slot] -= bytes;
            self.later[(slot + 1).max(index).min(SAMPLES) - 1] += bytes;
        }
    }
}

struct Walker<'a, R: Read> {
    reader: &'a mut JsonStreamReader<R>,
    /// Stored strings read only as a prefix, whose whole extents these are.
    clipped: &'a Clipped,
    accounting: Accounting,
    presented: &'a Presented,
    cancellation: &'a crate::job::CancellationToken,
    /// Compact JSON bytes of the pooled budgeted values with strings clipped to
    /// a sample, by where fits show them: no fit showing them is smaller.
    tally: Tally,
    /// Bytes pooled of budgeted values, which bound the pool's memory.
    memory: usize,
    /// Whether any element or member was left out of the pool.
    omitted: bool,
    /// Bytes of text fields so far, within `TEXT_ALLOWANCE`.
    text_bytes: usize,
}

impl<R: Read> Walker<'_, R> {
    /// The shape of the next value at `field`, and its pooled node. `record`
    /// holds while `field` is reached only through record members.
    fn walk(
        &mut self,
        field: &FieldPointer,
        record: bool,
        place: Place,
    ) -> Result<(Shape, Node), ToolError> {
        if self.cancellation.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        if self.presented.complete.contains(field) {
            let mut bytes = Vec::new();
            let mut writer = JsonStreamWriter::new(&mut bytes);
            self.reader.transfer_to(&mut writer).map_err(saved_json)?;
            writer.finish_document().map_err(saved_json)?;
            let value: Value = serde_json::from_slice(&bytes)?;
            return Ok((Shape::of(&value), Node::Complete(value)));
        }
        Ok(match self.reader.peek().map_err(saved_json)? {
            ValueType::Object => self.object(field, record, place)?,
            ValueType::Array => {
                self.reader.begin_array().map_err(saved_json)?;
                self.count(2, place);
                let (mut element, mut items, mut total) = (Shape::default(), Vec::new(), 0);
                while self.reader.has_next().map_err(saved_json)? {
                    let child_place = place.child(total);
                    let pooled = self.pools(child_place);
                    let child =
                        (pooled || !self.presented.complete.is_empty()).then(|| field.index(total));
                    match child {
                        Some(child) if pooled || self.protects(&child) => {
                            self.count(usize::from(total > 0), child_place);
                            let (shape, node) = self.walk(&child, false, child_place)?;
                            element.merge(shape);
                            items.push((total, node));
                        }
                        _ => {
                            self.omitted = true;
                            element.merge(self.shape()?);
                        }
                    }
                    total += 1;
                }
                self.reader.end_array().map_err(saved_json)?;
                (Shape::array(total, element), Node::Array { items, total })
            }
            ValueType::String => {
                let sample = self.sample();
                let text_field = record
                    && self.accounting == Accounting::Preview
                    && self.text_bytes < TEXT_ALLOWANCE;
                let keep = keep(self.presented, Some(field));
                let limit = if text_field { TEXT_BYTES } else { sample };
                let mut text = self.text(limit, text_field && keep == Keep::Ends)?;
                if let Some(clipped) = self.clipped.get(field) {
                    let size = |count: u64| usize::try_from(count).unwrap_or(usize::MAX);
                    text.bytes = size(clipped.extent.bytes);
                    text.newlines = size(clipped.extent.newlines);
                    text.ends_line = clipped.extent.ends_line;
                    text.end.clone_from(&clipped.end);
                }
                text.role = role(&mut self.text_bytes, self.accounting, record, keep, &text);
                if text.role == TextRole::Sample {
                    text.prefix.truncate(sample + 1);
                    text.end = Vec::new();
                    text.limit = text.limit.min(sample);
                    self.count_text(&text, place);
                }
                (Shape::scalar(Scalar::String), Node::Text(text))
            }
            ValueType::Number => {
                let token = self.reader.next_number_as_str().map_err(saved_json)?;
                let shape = Shape::scalar(Scalar::number(token));
                let value: Value = serde_json::from_str(token)?;
                // Shown as the parsed number, however the source spelled it.
                self.count(json_bytes(&value), place);
                (shape, Node::Value(value))
            }
            ValueType::Boolean => {
                let value = self.reader.next_bool().map_err(saved_json)?;
                self.count(if value { 4 } else { 5 }, place);
                (Shape::scalar(Scalar::Boolean), Node::Value(value.into()))
            }
            ValueType::Null => {
                self.reader.next_null().map_err(saved_json)?;
                self.count(4, place);
                (Shape::scalar(Scalar::Null), Node::Value(Value::Null))
            }
        })
    }

    /// An object's members are counted at the object's rank, as a record's are,
    /// until it proves a collection; then those counted so far move to the ranks
    /// their positions give them.
    fn object(
        &mut self,
        field: &FieldPointer,
        record: bool,
        place: Place,
    ) -> Result<(Shape, Node), ToolError> {
        self.reader.begin_object().map_err(saved_json)?;
        self.count(2, place);
        let mut shape = ObjectBuilder::default();
        let (mut members, mut total) = (Vec::new(), 0);
        let mut counted: Vec<Tally> = Vec::new();
        while self.reader.has_next().map_err(saved_json)? {
            let key = self.reader.next_name_owned().map_err(saved_json)?;
            if total == COLLECTION_MEMBERS {
                for (index, delta) in counted.iter().enumerate() {
                    self.tally.rerank(delta, index);
                }
                for (index, _, member) in &mut members {
                    self.demote(member, place.child(*index));
                }
            }
            let leading = total < COLLECTION_MEMBERS;
            // Until it proves a collection, an object's members are taken as a
            // record's; a text field among them only uses up the allowance.
            let (member_record, member_place) = if leading {
                (record, place.member())
            } else {
                (false, place.child(total))
            };
            let pooled = self.pools(member_place);
            let child = (pooled || leading || !self.presented.complete.is_empty())
                .then(|| field.property(&key));
            let before = self.tally.clone();
            let member = match child {
                Some(child) if pooled || leading || self.protects(&child) => {
                    // Quotes, colon and the comma before all but the first.
                    self.count(key.len() + 3 + usize::from(total > 0), member_place);
                    let (member, node) = if pooled || self.protects(&child) {
                        self.walk(&child, member_record, member_place)?
                    } else {
                        self.stub(&child, member_record, member_place)?
                    };
                    members.push((total, key.clone(), node));
                    member
                }
                _ => {
                    self.omitted = true;
                    self.shape()?
                }
            };
            if leading {
                counted.push(self.tally.since(&before));
            }
            shape.member(&key, member);
            total += 1;
        }
        self.reader.end_object().map_err(saved_json)?;
        Ok((shape.finish(), Node::Object { members, total }))
    }

    /// A record member beyond the pool, so that the record keeps every member:
    /// a scalar as usual, a container emptied but for its count.
    fn stub(
        &mut self,
        field: &FieldPointer,
        record: bool,
        place: Place,
    ) -> Result<(Shape, Node), ToolError> {
        let object = match self.reader.peek().map_err(saved_json)? {
            ValueType::Object => true,
            ValueType::Array => false,
            _ => return self.walk(field, record, place),
        };
        self.omitted = true;
        let (shape, total) = self.scan()?;
        let node = if object {
            Node::Object {
                members: Vec::new(),
                total,
            }
        } else {
            Node::Array {
                items: Vec::new(),
                total,
            }
        };
        Ok((shape, node))
    }

    /// Make the text fields a collection's leading members were pooled with, as
    /// a record's, samples at `place`, returning their allowance.
    fn demote(&mut self, node: &mut Node, place: Place) {
        match node {
            Node::Text(text) if matches!(text.role, TextRole::Field(_)) => {
                self.text_bytes -= text.prefix.len().min(TEXT_BYTES);
                let sample = self.sample();
                text.role = TextRole::Sample;
                text.prefix.truncate(sample + 1);
                text.end = Vec::new();
                text.limit = text.limit.min(sample);
                self.count_text(text, place);
            }
            // Only records hold text fields: nothing under an array does.
            Node::Object { members, .. } => {
                for (_, _, member) in members {
                    self.demote(member, place.member());
                }
            }
            Node::Array { .. } | Node::Text(_) | Node::Value(_) | Node::Complete(_) => {}
        }
    }

    /// The shape of the next value, which is not pooled, so needs no pointers.
    fn shape(&mut self) -> Result<Shape, ToolError> {
        Ok(self.scan()?.0)
    }

    /// The shape of the next value, and how many members or elements it has.
    fn scan(&mut self) -> Result<(Shape, usize), ToolError> {
        if self.cancellation.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        let mut total = 0;
        let shape = match self.reader.peek().map_err(saved_json)? {
            ValueType::Object => {
                self.reader.begin_object().map_err(saved_json)?;
                let mut shape = ObjectBuilder::default();
                while self.reader.has_next().map_err(saved_json)? {
                    let key = self.reader.next_name_owned().map_err(saved_json)?;
                    let member = self.shape()?;
                    shape.member(&key, member);
                    total += 1;
                }
                self.reader.end_object().map_err(saved_json)?;
                shape.finish()
            }
            ValueType::Array => {
                self.reader.begin_array().map_err(saved_json)?;
                let mut element = Shape::default();
                while self.reader.has_next().map_err(saved_json)? {
                    element.merge(self.shape()?);
                    total += 1;
                }
                self.reader.end_array().map_err(saved_json)?;
                Shape::array(total, element)
            }
            ValueType::Number => Shape::scalar(Scalar::number(
                self.reader.next_number_as_str().map_err(saved_json)?,
            )),
            ValueType::String => {
                self.reader.skip_value().map_err(saved_json)?;
                Shape::scalar(Scalar::String)
            }
            ValueType::Boolean => {
                self.reader.skip_value().map_err(saved_json)?;
                Shape::scalar(Scalar::Boolean)
            }
            ValueType::Null => {
                self.reader.skip_value().map_err(saved_json)?;
                Shape::scalar(Scalar::Null)
            }
        };
        Ok((shape, total))
    }

    /// The most bytes pooled of a string shown as a sample: a pool beyond the
    /// budget rules out a whole fit, so later strings are only ever shown as a
    /// sample's prefix.
    fn sample(&self) -> usize {
        if self.memory < self.accounting.budget() {
            self.accounting.budget()
        } else {
            SAMPLE_TEXT_BYTES[0]
        }
    }

    /// Whether a candidate at `place` is pooled.
    fn pools(&self, place: Place) -> bool {
        if place.rank < SAMPLES {
            self.tally.before(place) < POOL_BYTES
        } else {
            self.memory < POOL_BYTES && self.tally.total() < self.accounting.budget()
        }
    }

    fn count(&mut self, bytes: usize, place: Place) {
        self.tally.add(place, bytes);
        self.memory += bytes;
    }

    /// A sample string, counted as a fit would show it at most.
    fn count_text(&mut self, text: &Text, place: Place) {
        let pooled = text.prefix.len();
        self.tally.add(place, pooled.min(SAMPLE_TEXT_BYTES[0]) + 2);
        self.memory += pooled + 2;
    }

    fn protects(&self, field: &FieldPointer) -> bool {
        protects(&self.presented.complete, field)
    }

    /// The next string's prefix and extent, with its last bytes when `end`.
    fn text(&mut self, limit: usize, end: bool) -> Result<Text, ToolError> {
        let mut input = self.reader.next_string_reader().map_err(saved_json)?;
        let mut text = Text {
            role: TextRole::Sample,
            prefix: Vec::new(),
            end: Vec::new(),
            limit,
            bytes: 0,
            newlines: 0,
            ends_line: false,
        };
        let mut buffer = [0; 8 * 1024];
        loop {
            if self.cancellation.is_cancelled() {
                return Err(ToolError::cancelled());
            }
            let count = input.read(&mut buffer)?;
            let Some(last) = buffer[..count].last() else {
                return Ok(text);
            };
            let chunk = &buffer[..count];
            let room = (limit + 1).saturating_sub(text.prefix.len()).min(count);
            text.prefix.extend_from_slice(&chunk[..room]);
            if end {
                text.end.extend_from_slice(chunk);
                let excess = text.end.len().saturating_sub(TEXT_BYTES);
                text.end.drain(..excess);
            }
            text.bytes += count;
            text.newlines += chunk.iter().filter(|&&byte| byte == b'\n').count();
            text.ends_line = *last == b'\n';
        }
    }
}
