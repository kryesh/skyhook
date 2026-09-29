//! What one fit shows of a pooled value, with the records of what it leaves out.
use std::collections::BTreeSet;

use serde_json::Value;

use super::{
    LISTED_CUTS, TEXT_BYTES, TEXT_LINES, json_bytes,
    pool::{Node, Text, TextRole},
    protects,
};
use crate::job::output::{FieldPointer, OutputTruncation, shape::COLLECTION_MEMBERS};

/// How much of each pooled value a fit shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Fit {
    pub(super) samples: Samples,
    /// Bytes kept of a string inside a sample; `None` keeps all that was pooled.
    pub(super) clip: Option<usize>,
    /// Containers at this depth or deeper are left empty; `None` for any depth.
    pub(super) depth: Option<usize>,
}

impl Fit {
    pub(super) const WHOLE: Self = Self {
        samples: Samples::Every,
        clip: None,
        depth: None,
    };
}

/// The children shown of each array or collection.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Samples {
    Every,
    First(usize),
}

impl Samples {
    fn include(self, index: usize) -> bool {
        match self {
            Self::Every => true,
            Self::First(count) => index < count,
        }
    }
}

/// What a fit shows of a value.
pub(super) struct Shown {
    pub(super) value: Value,
    /// Text fields' records, then cuts within samples.
    pub(super) truncated: Vec<OutputTruncation>,
    /// Whether any array or object was cut.
    pub(super) cut: bool,
    /// Bytes of text fields and complete values, which the budget leaves out.
    pub(super) exempt: usize,
}

pub(super) fn present(
    node: &Node,
    field: &FieldPointer,
    fit: Fit,
    complete: &BTreeSet<FieldPointer>,
) -> Shown {
    let mut presenter = Presenter {
        fit,
        complete,
        fields: Vec::new(),
        cuts: Vec::new(),
        exempt: 0,
    };
    let value = presenter.present(node, field, 0);
    let cut = (presenter.cuts.iter()).any(|cut| !matches!(cut, OutputTruncation::Text { .. }));
    let mut truncated = presenter.fields;
    truncated.extend(summarize(presenter.cuts));
    Shown {
        value,
        truncated,
        cut,
        exempt: presenter.exempt,
    }
}

struct Presenter<'a> {
    fit: Fit,
    complete: &'a BTreeSet<FieldPointer>,
    fields: Vec<OutputTruncation>,
    /// Cuts within samples, each container's before those inside it.
    cuts: Vec<OutputTruncation>,
    exempt: usize,
}

impl Presenter<'_> {
    fn present(&mut self, node: &Node, field: &FieldPointer, depth: usize) -> Value {
        match node {
            Node::Value(value) => value.clone(),
            Node::Complete(value) => {
                self.exempt += json_bytes(value);
                value.clone()
            }
            Node::Text(text) => match text.role {
                TextRole::Field => {
                    let (value, cut) = shorten(text, field, TEXT_BYTES, Some(TEXT_LINES));
                    self.exempt += json_bytes(&value);
                    self.fields.extend(cut);
                    value
                }
                TextRole::Sample => {
                    let clip = self.fit.clip.unwrap_or(text.limit);
                    let (value, cut) = shorten(text, field, clip, None);
                    self.cuts.extend(cut);
                    value
                }
            },
            Node::Array { items, total } => {
                let children = items
                    .iter()
                    .map(|(index, node)| (*index, field.index(*index), node));
                let sampled = |index| self.within(depth) && self.fit.samples.include(index);
                let shown = self.shown(children, sampled);
                if shown.len() < *total {
                    self.cuts.push(OutputTruncation::Elements {
                        field: field.clone(),
                        shown: shown.len(),
                        total_elements: *total,
                        kept: kept(&shown),
                    });
                }
                Value::Array(
                    (shown.iter())
                        .map(|(_, child, node)| self.present(node, child, depth + 1))
                        .collect(),
                )
            }
            Node::Object { members, total } => {
                let children = members
                    .iter()
                    .map(|(index, key, node)| (*index, field.property(key), (key, node)));
                // A record shows all its members, or none but those holding
                // complete fields when deeper than the fit allows.
                let shown = if *total > COLLECTION_MEMBERS {
                    self.shown(children, |index| {
                        self.within(depth) && self.fit.samples.include(index)
                    })
                } else {
                    let whole = self.within(depth);
                    self.shown(children, |_| whole)
                };
                if shown.len() < *total {
                    self.cuts.push(OutputTruncation::Members {
                        field: field.clone(),
                        shown: shown.len(),
                        total_members: *total,
                        kept: kept(&shown),
                    });
                }
                Value::Object(
                    (shown.iter())
                        .map(|(_, child, (key, node))| {
                            ((*key).clone(), self.present(node, child, depth + 1))
                        })
                        .collect(),
                )
            }
        }
    }

    /// The children `include` selects by source index, and any holding complete
    /// fields.
    fn shown<T>(
        &self,
        children: impl Iterator<Item = (usize, FieldPointer, T)>,
        include: impl Fn(usize) -> bool,
    ) -> Vec<(usize, FieldPointer, T)> {
        children
            .filter(|(index, child, _)| include(*index) || self.protects(child))
            .collect()
    }

    /// Whether a container at `depth` shows its children.
    fn within(&self, depth: usize) -> bool {
        self.fit.depth.is_none_or(|limit| depth < limit)
    }

    fn protects(&self, field: &FieldPointer) -> bool {
        protects(self.complete, field)
    }
}

/// Source index ranges of shown children, when they are not a leading run.
fn kept<T>(shown: &[(usize, FieldPointer, T)]) -> Option<Vec<[usize; 2]>> {
    let contiguous = shown
        .iter()
        .enumerate()
        .all(|(position, (index, ..))| position == *index);
    (!contiguous).then(|| {
        let mut ranges: Vec<[usize; 2]> = Vec::new();
        for (index, ..) in shown {
            match ranges.last_mut() {
                Some(range) if range[1] + 1 == *index => range[1] = *index,
                _ => ranges.push([*index, *index]),
            }
        }
        ranges
    })
}

/// The prefix of `text` kept within `bytes` (and `lines`, preferring whole lines
/// there), and its record when it is shorter than the string.
fn shorten(
    text: &Text,
    field: &FieldPointer,
    bytes: usize,
    lines: Option<usize>,
) -> (Value, Option<OutputTruncation>) {
    let prefix = &text.prefix;
    let line_end = lines.and_then(|lines| {
        prefix
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .nth(lines - 1)
            .map(|(index, _)| index + 1)
    });
    let mut end = bytes.min(prefix.len()).min(line_end.unwrap_or(usize::MAX));
    end = match std::str::from_utf8(&prefix[..end]) {
        Ok(_) => end,
        Err(error) => error.valid_up_to(),
    };
    if lines.is_some()
        && end < text.bytes
        && let Some(last) = prefix[..end].iter().rposition(|&byte| byte == b'\n')
    {
        end = last + 1;
    }
    // A cut between CR and LF would split one line terminator across pages.
    if end > 0 && prefix.get(end) == Some(&b'\n') && prefix[end - 1] == b'\r' {
        end -= 1;
    }
    let kept = String::from_utf8_lossy(&prefix[..end]).into_owned();
    if end >= text.bytes {
        return (kept.into(), None);
    }
    let line = 1 + prefix[..end].iter().filter(|&&byte| byte == b'\n').count();
    let offset =
        (prefix[..end].iter().rposition(|&byte| byte == b'\n')).map_or(end, |last| end - last - 1);
    let record = OutputTruncation::Text {
        field: field.clone(),
        total_lines: text.newlines + usize::from(text.bytes > 0 && !text.ends_line),
        next_start: line,
        next_offset: (offset != 0).then_some(offset),
    };
    (kept.into(), Some(record))
}

/// At most `LISTED_CUTS` records: beyond them, one summary at the remaining cuts'
/// nearest common parent. Records mapping shown values to source indices are
/// always listed.
fn summarize(cuts: Vec<OutputTruncation>) -> Vec<OutputTruncation> {
    let (mut listed, mut rest): (Vec<_>, Vec<_>) = cuts.into_iter().partition(|cut| {
        matches!(
            cut,
            OutputTruncation::Elements { kept: Some(_), .. }
                | OutputTruncation::Members { kept: Some(_), .. }
        )
    });
    let room = LISTED_CUTS.saturating_sub(listed.len()).max(1);
    if rest.len() <= room {
        listed.append(&mut rest);
        return listed;
    }
    let summarized = rest.split_off(room - 1);
    listed.append(&mut rest);
    let mut parent: Vec<String> = summarized[0].field().segments().collect();
    for cut in &summarized[1..] {
        let common = parent
            .iter()
            .zip(cut.field().segments())
            .take_while(|(left, right)| *left == right)
            .count();
        parent.truncate(common);
    }
    let field = parent
        .iter()
        .fold(FieldPointer::root(), |pointer, segment| {
            pointer.property(segment)
        });
    listed.push(OutputTruncation::Summary {
        field,
        cuts: summarized.len(),
    });
    listed
}
