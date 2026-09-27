//! Automatic previews of saved results: one streaming pass collects the value's
//! exhaustive shape and a bounded pool of samples, then the samples are fitted to
//! the preview budget in memory.
//!
//! Strings reached only through record members keep the per-field text limit
//! outside the budget, until together they use up the text allowance, and complete
//! fields are shown whole outside it. Arrays and collection objects share one
//! sample count; strings inside samples are clipped.
mod pool;
mod present;

use std::{collections::BTreeSet, io::Read};

use serde_json::Value;

use pool::Node;
pub(super) use pool::pool;
use present::{Fit, Samples};

use super::{
    CONTENT_BYTES, FieldPointer, OutputTruncation, ToolError, json, render::Clipped, shape::Shape,
};

/// Budget for a preview's sampled content, its shape and its truncation records.
const PREVIEW_BUDGET: usize = 8 * 1024;
/// Budget for everything one page shows.
const PAGE_BUDGET: usize = CONTENT_BYTES;
/// A text field keeps at most this many bytes, as much as the budget, and lines.
pub(super) const TEXT_BYTES: usize = PREVIEW_BUDGET;
pub(super) const TEXT_LINES: usize = 100;
/// Text fields together keep at most this many bytes outside the budget; later
/// strings are shortened within it.
const TEXT_ALLOWANCE: usize = PAGE_BUDGET;
/// No fit shows more of one string than a page holds.
pub(super) const STRING_BYTES: usize = PAGE_BUDGET;
/// A string inside a sample keeps at most this many bytes, then less if needed.
pub(super) const SAMPLE_TEXT_BYTES: [usize; 3] = [256, 64, 16];
/// The most samples shown of one array or collection; the shape describes the rest.
const SAMPLES: usize = 10;
/// Sample cuts listed individually before the rest are summarized.
const LISTED_CUTS: usize = 32;

pub(super) struct Preview {
    pub(super) value: Value,
    /// Present when any array or collection was cut.
    pub(super) shape: Option<Value>,
    pub(super) truncated: Vec<OutputTruncation>,
}

/// Who a fit is for, which sets what is pooled and what its budget counts.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Accounting {
    /// An automatic preview: text fields keep their own limits and, with complete
    /// values, sit outside the budget.
    Preview,
    /// An explicit page: everything shown counts, and every string is a sample.
    Page,
}

impl Accounting {
    /// The bytes a fit may show, outside what it exempts.
    pub(super) fn budget(self) -> usize {
        match self {
            Self::Preview => PREVIEW_BUDGET,
            Self::Page => PAGE_BUDGET,
        }
    }

    /// The most bytes a shape takes, which leaves the rest of the budget to at
    /// least the outermost level of the value.
    pub(super) fn shape_bytes(self) -> usize {
        self.budget() / 2
    }
}

/// Preview the JSON text `input` of the value at `field`. A value within the
/// budget is only subject to the text-field limits.
pub(super) fn preview(
    mut input: impl Read,
    field: &FieldPointer,
    complete: &BTreeSet<FieldPointer>,
    clipped: &Clipped,
    cancellation: &crate::job::CancellationToken,
) -> Result<Preview, ToolError> {
    let mut head = Vec::new();
    (&mut input)
        .take(PREVIEW_BUDGET as u64 + 1)
        .read_to_end(&mut head)?;
    let pooled = if head.len() <= PREVIEW_BUDGET {
        let value: Value = serde_json::from_slice(&head)?;
        let shape = Shape::of(&value);
        Pooled::new(
            shape,
            Node::of(value, field, complete, Accounting::Preview),
            true,
            Accounting::Preview,
        )
    } else {
        let mut reader = json::reader(std::io::Cursor::new(head).chain(input));
        pool(
            &mut reader,
            field,
            complete,
            clipped,
            cancellation,
            Accounting::Preview,
        )?
    };
    Ok(pooled.fitted(field, complete, PREVIEW_BUDGET))
}

/// A value's exhaustive shape and pooled samples, ready to fit a budget.
pub(super) struct Pooled {
    shape: Shape,
    rendered: std::cell::OnceCell<Value>,
    node: Node,
    /// Whether every element and member of the value was pooled.
    covered: bool,
    accounting: Accounting,
}

impl Pooled {
    /// An in-memory value for a page, held whole.
    pub(super) fn of(value: Value) -> Self {
        let shape = Shape::of(&value);
        let node = Node::of(
            value,
            &FieldPointer::root(),
            &BTreeSet::new(),
            Accounting::Page,
        );
        Self::new(shape, node, true, Accounting::Page)
    }

    fn new(shape: Shape, node: Node, covered: bool, accounting: Accounting) -> Self {
        Self {
            shape,
            rendered: std::cell::OnceCell::new(),
            node,
            covered,
            accounting,
        }
    }

    /// The whole value for a page; none when it was not all pooled. Strings
    /// read as a prefix are still cut.
    pub(super) fn whole(&self, field: &FieldPointer) -> Option<Preview> {
        (self.covered).then(|| self.attempt(field, &BTreeSet::new(), Fit::WHOLE).0)
    }

    /// The pooled value fitted to `budget`: whole, else every element with
    /// shortened strings, else the largest shared sample count, then shorter
    /// strings and shallower samples when a single sample does not fit. Leaving
    /// the value's outermost level empty always fits, apart from complete fields
    /// and their positions.
    pub(super) fn fitted(
        &self,
        field: &FieldPointer,
        complete: &BTreeSet<FieldPointer>,
        budget: usize,
    ) -> Preview {
        let [clip, shorter @ ..] = SAMPLE_TEXT_BYTES;
        let deepest = |samples, clip| Fit {
            samples,
            clip: Some(clip),
            depth: None,
        };
        let every = (self.covered)
            .then_some([Fit::WHOLE, deepest(Samples::Every, clip)])
            .into_iter()
            .flatten();
        // Showing more samples can cost less: records of fully shown arrays, and
        // without any cut the shape, drop out. Every count is tried.
        let counts = (1..=SAMPLES)
            .rev()
            .map(|count| deepest(Samples::First(count), clip));
        let shorter = shorter
            .into_iter()
            .map(|clip| deepest(Samples::First(1), clip));
        let [.., shortest] = SAMPLE_TEXT_BYTES;
        let shallower = |depth| Fit {
            samples: Samples::First(1),
            clip: Some(shortest),
            depth: Some(depth),
        };
        every
            .chain(counts)
            .chain(shorter)
            .chain((1..self.node.depth()).rev().map(shallower))
            .map(|fit| self.attempt(field, complete, fit))
            .find(|(_, bytes)| *bytes <= budget)
            .map_or_else(
                || self.attempt(field, complete, shallower(0)).0,
                |(preview, _)| preview,
            )
    }

    fn attempt(
        &self,
        field: &FieldPointer,
        complete: &BTreeSet<FieldPointer>,
        fit: Fit,
    ) -> (Preview, usize) {
        let shown = present::present(&self.node, field, fit, complete);
        let shape = shown.cut.then(|| {
            let bytes = self.accounting.shape_bytes();
            self.rendered
                .get_or_init(|| self.shape.render(bytes))
                .clone()
        });
        let bytes = (json_bytes(&shown.value)
            + json_bytes(&shown.truncated)
            + shape.as_ref().map_or(0, json_bytes))
        .saturating_sub(shown.exempt);
        let preview = Preview {
            value: shown.value,
            shape,
            truncated: shown.truncated,
        };
        (preview, bytes)
    }
}

/// Whether `field` is complete or holds a complete field.
fn protects(complete: &BTreeSet<FieldPointer>, field: &FieldPointer) -> bool {
    complete
        .iter()
        .any(|complete| complete == field || field.contains(complete))
}

pub(super) fn json_bytes(value: &impl serde::Serialize) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::output::shape::COLLECTION_MEMBERS;
    use serde_json::json;

    fn run(value: &Value, complete: &[&str]) -> Preview {
        let complete = complete
            .iter()
            .map(|field| field.parse().unwrap())
            .collect();
        let bytes = serde_json::to_vec(value).unwrap();
        let cancellation = crate::job::CancellationToken::new();
        let clipped = Clipped::new();
        preview(
            bytes.as_slice(),
            &FieldPointer::result(),
            &complete,
            &clipped,
            &cancellation,
        )
        .unwrap()
    }

    /// Bytes the budget counts, when no text field or complete value is shown.
    fn budgeted(preview: &Preview) -> usize {
        json_bytes(&preview.value) + json_bytes(&preview.shape) + json_bytes(&preview.truncated)
    }

    fn cut<'a>(preview: &'a Preview, field: &str) -> &'a OutputTruncation {
        preview
            .truncated
            .iter()
            .find(|cut| cut.field().as_str() == field)
            .unwrap_or_else(|| panic!("no cut at {field}: {:?}", preview.truncated))
    }

    #[test]
    fn values_within_the_budget_are_whole_except_long_text_fields() {
        // The text field alone exceeds the budget, which it sits outside.
        let items: Vec<Vec<usize>> = vec![(0..300).collect()];
        let log = "line\n".repeat(10_000);
        let value = json!({"notes": ["x".repeat(5000)], "items": items, "log": log});
        let preview = run(&value, &[]);
        assert_eq!(preview.value["notes"], value["notes"]);
        assert_eq!(preview.value["items"], value["items"]);
        assert_eq!(preview.value["log"], "line\n".repeat(TEXT_LINES));
        assert!(preview.shape.is_none());
        assert!(matches!(
            cut(&preview, "/result/log"),
            OutputTruncation::Text {
                total_lines: 10_000,
                next_start: 101,
                next_offset: None,
                ..
            }
        ));
        assert_eq!(preview.truncated.len(), 1);
    }

    #[test]
    fn text_fields_share_one_allowance_outside_the_budget() {
        let data: serde_json::Map<_, _> = (0..100)
            .map(|index| (format!("k{index:02}"), json!("v".repeat(7 * 1024))))
            .collect();
        let preview = run(&json!({"data": data}), &[]);
        let shown = preview.value["data"].as_object().unwrap();
        let whole = (shown.values()).filter(|text| text.as_str().unwrap().len() == 7 * 1024);
        assert_eq!(whole.count(), TEXT_ALLOWANCE / (7 * 1024));
        assert!(budgeted(&preview) <= TEXT_ALLOWANCE + PREVIEW_BUDGET);
    }

    #[test]
    fn records_keep_every_member_however_heavy_their_first_sample() {
        let inner: serde_json::Map<_, _> = (0..100)
            .map(|index| (format!("g{index:02}"), json!(1_234_567_890)))
            .collect();
        let heavy: serde_json::Map<_, _> = (0..100)
            .map(|index| (format!("m{index:02}"), json!(inner)))
            .collect();
        let value = json!({"stdout": "done\n", "items": [heavy], "exit_code": 0});
        let preview = run(&value, &[]);
        assert_eq!(preview.value["stdout"], "done\n");
        assert_eq!(preview.value["exit_code"], 0);
        assert!(budgeted(&preview) <= PREVIEW_BUDGET);
    }

    #[test]
    fn deep_detail_never_crowds_out_a_later_first_sample() {
        let inner: serde_json::Map<_, _> = (0..100)
            .map(|index| (format!("g{index:02}"), json!(1_234_567_890)))
            .collect();
        let heavy: serde_json::Map<_, _> = (0..100)
            .map(|index| (format!("m{index:02}"), json!(inner)))
            .collect();
        let preview = run(&json!({"a": [heavy], "b": [42, 43, 44]}), &[]);
        assert_eq!(preview.value["b"], json!([42]));
        assert!(budgeted(&preview) <= PREVIEW_BUDGET);
    }

    #[test]
    fn a_collections_strings_are_samples_leaving_the_allowance_to_text_fields() {
        let labels: serde_json::Map<_, _> = (0..150)
            .map(|index| (format!("m{index:03}"), json!("v".repeat(1000))))
            .collect();
        // A text field needing more allowance than the collection would have left.
        let log = "a line of the log\n".repeat(TEXT_LINES);
        let preview = run(&json!({"labels": labels, "stdout": log}), &[]);
        assert_eq!(preview.value["stdout"], log);
        let shown = preview.value["labels"].as_object().unwrap();
        assert!(!shown.is_empty());
        assert!(
            shown
                .values()
                .all(|text| text.as_str().unwrap().len() <= SAMPLE_TEXT_BYTES[0])
        );
    }

    #[test]
    fn every_element_shows_when_shortening_strings_is_enough() {
        let mut items: Vec<_> = (0..20).map(|id| json!({"id": id})).collect();
        items[0]["log"] = json!("x".repeat(20_000));
        let preview = run(&Value::Array(items), &[]);
        assert_eq!(preview.value.as_array().unwrap().len(), 20);
        assert!(preview.shape.is_none());
        assert!(matches!(
            &preview.truncated[..],
            [OutputTruncation::Text { field, .. }] if field.as_str() == "/result/0/log"
        ));
        // Long strings pool only what a sample shows once no whole fit remains.
        let preview = run(&json!(vec!["x".repeat(10_000); 20]), &[]);
        let shown = preview.value.as_array().unwrap();
        assert_eq!(shown.len(), 20);
        assert!(
            shown
                .iter()
                .all(|text| text.as_str().unwrap().len() == SAMPLE_TEXT_BYTES[0])
        );
        assert!(preview.shape.is_none());
        // Where they do not all fit, the sample count still applies.
        let preview = run(&json!(vec!["x".repeat(10_000); 100]), &[]);
        let shown = preview.value.as_array().unwrap().len();
        assert!((1..=SAMPLES).contains(&shown), "{shown}");
    }

    #[test]
    fn earlier_candidates_never_crowd_out_a_later_first_sample() {
        let record: serde_json::Map<_, _> = (0..100)
            .map(|index| (format!("f{index}"), json!("x".repeat(1000))))
            .collect();
        let preview = run(&json!({"a": vec![record; 10], "b": [42, 43, 44]}), &[]);
        assert_eq!(preview.value["a"].as_array().unwrap().len(), 1);
        assert_eq!(preview.value["b"], json!([42]));
        assert!(budgeted(&preview) <= PREVIEW_BUDGET);
        // A collection's leading members count as its first sample only until it
        // proves a collection.
        let member: serde_json::Map<_, _> = (0..10)
            .map(|index| (format!("s{index}"), json!("x".repeat(300))))
            .collect();
        let collection: serde_json::Map<_, _> = (0..150)
            .map(|index| (format!("m{index:03}"), json!(member)))
            .collect();
        let preview = run(&json!({"a": [collection], "b": [42, 43, 44]}), &[]);
        let shown = preview.value["a"][0].as_object().unwrap().len();
        assert!(shown > 0);
        assert_eq!(preview.value["b"], json!([42, 43, 44][..shown]));
    }

    #[test]
    fn shapes_leave_room_for_the_value() {
        // Each sample's many long member names make its shape nearly the budget.
        let record: serde_json::Map<_, _> = (0..90)
            .map(|index| (format!("{index:0>80}"), json!(index)))
            .collect();
        let preview = run(&json!(vec![record; 1000]), &[]);
        assert!(json_bytes(&preview.shape) <= PREVIEW_BUDGET / 2);
        assert!(budgeted(&preview) <= PREVIEW_BUDGET);
        assert!(matches!(
            cut(&preview, "/result"),
            OutputTruncation::Elements {
                total_elements: 1000,
                ..
            }
        ));
    }

    #[test]
    fn large_values_show_an_exhaustive_shape_and_samples_fitted_to_the_budget() {
        let items: Vec<_> = (0..3000)
            .map(|index| {
                json!({
                    "metadata": {"name": format!("pod-{index}"), "labels": {"app": "api"}},
                    "spec": {"containers": ["a", "b", "c"]},
                    "status": {"phase": "Running"},
                })
            })
            .collect();
        let preview = run(&json!({"cluster": "prod", "items": items}), &[]);
        assert_eq!(
            preview.shape,
            Some(json!({
                "cluster": "string",
                "items": [3000, {
                    "metadata": {"name": "string", "labels": {"app": "string"}},
                    "spec": {"containers": [3, "string"]},
                    "status": {"phase": "string"},
                }],
            }))
        );
        let shown = preview.value["items"].as_array().unwrap().len();
        assert!((1..=SAMPLES).contains(&shown), "{shown}");
        assert_eq!(preview.value["items"][0], items[0]);
        assert!(matches!(
            cut(&preview, "/result/items"),
            OutputTruncation::Elements { shown: listed, total_elements: 3000, kept: None, .. }
                if *listed == shown
        ));
        // The record's own text field sits outside the budget.
        let text = json_bytes(&preview.value["cluster"]);
        assert!(budgeted(&preview) - text <= PREVIEW_BUDGET);
    }

    #[test]
    fn collections_lose_members_while_records_keep_theirs() {
        let record: serde_json::Map<_, _> = (0..COLLECTION_MEMBERS)
            .map(|index| (format!("field-{index}"), json!(index)))
            .collect();
        let collection: serde_json::Map<_, _> = (0..5000)
            .map(|index| (format!("label-{index:04}"), json!("v".repeat(20))))
            .collect();
        let preview = run(&json!({"record": record, "labels": collection}), &[]);
        assert_eq!(preview.value["record"], json!(record));
        let shown = preview.value["labels"].as_object().unwrap().len();
        assert!(shown < 5000);
        assert_eq!(preview.value["labels"]["label-0000"], "v".repeat(20));
        assert!(matches!(
            cut(&preview, "/result/labels"),
            OutputTruncation::Members {
                total_members: 5000,
                ..
            }
        ));
        assert_eq!(
            preview.shape.unwrap()["labels"],
            json!({"*": "string", "#": 5000})
        );
    }

    #[test]
    fn complete_fields_stay_whole_at_their_source_positions() {
        let mut items: Vec<_> = (0..2000).map(|index| json!({"id": index})).collect();
        let answer = "a".repeat(3 * PREVIEW_BUDGET);
        items[1500] = json!({"id": 1500, "answer": answer});
        let preview = run(&json!({"items": items}), &["/result/items/1500/answer"]);
        let shown = preview.value["items"].as_array().unwrap();
        assert_eq!(shown.last().unwrap(), &items[1500]);
        let OutputTruncation::Elements {
            kept: Some(kept), ..
        } = cut(&preview, "/result/items")
        else {
            panic!("{:?}", preview.truncated)
        };
        assert_eq!(kept.last(), Some(&[1500, 1500]));
        assert_eq!(kept[0][0], 0);
    }

    #[test]
    fn complete_fields_stay_whole_on_the_in_memory_path_too() {
        let instructions = "step\n".repeat(150);
        let preview = run(
            &json!({"instructions": instructions}),
            &["/result/instructions"],
        );
        assert_eq!(preview.value["instructions"], instructions);
        assert!(preview.truncated.is_empty());
    }

    #[test]
    fn records_keep_every_member_and_mappings_survive_summaries() {
        let record: serde_json::Map<_, _> = (0..40)
            .map(|index| (format!("f{index}"), json!("x".repeat(5000))))
            .collect();
        let preview = run(&json!([record]), &[]);
        assert_eq!(preview.value[0].as_object().unwrap().len(), 40);
        // Enough string cuts to summarize never hide which elements are shown.
        let mut items: Vec<_> = (0..2000)
            .map(|_| json!({"note": "n".repeat(300)}))
            .collect();
        items[1500] = json!({"answer": "a"});
        let preview = run(&json!({"items": items}), &["/result/items/1500/answer"]);
        assert!(matches!(
            cut(&preview, "/result/items"),
            OutputTruncation::Elements { kept: Some(kept), .. } if kept.last() == Some(&[1500, 1500])
        ));
    }

    #[test]
    fn massive_strings_are_read_past_to_a_prefix_with_their_whole_extent() {
        let log = "line\n".repeat(1_000_000);
        let rows: Vec<_> = (0..2000).map(|index| json!({"row": index})).collect();
        let preview = run(&json!({"logs": [log], "rows": rows}), &[]);
        let prefix = preview.value["logs"][0].as_str().unwrap();
        assert!(prefix.len() <= PREVIEW_BUDGET && log.starts_with(prefix));
        assert!(matches!(
            cut(&preview, "/result/logs/0"),
            OutputTruncation::Text {
                total_lines: 1_000_000,
                ..
            }
        ));
        assert!(budgeted(&preview) <= PREVIEW_BUDGET);
    }

    #[test]
    fn cuts_beyond_the_listed_count_are_summarized_at_their_common_parent() {
        // Each sample holds several cut arrays, more cuts in all than are listed.
        let values: Vec<_> = (0..200).collect();
        let items: Vec<_> = (0..200)
            .map(|_| json!({"a": values, "b": values, "c": values, "d": values, "e": values}))
            .collect();
        let preview = run(&json!({"items": items}), &[]);
        assert_eq!(preview.truncated.len(), LISTED_CUTS);
        // A container's own record, which continues it, comes before those inside.
        assert_eq!(preview.truncated[0].field().as_str(), "/result/items");
        assert!(matches!(
            preview.truncated.last().unwrap(),
            OutputTruncation::Summary { field, .. } if field.as_str() == "/result/items"
        ));
        assert!(budgeted(&preview) <= PREVIEW_BUDGET);
    }
}
