//! Candidate preference order and conservative equivalence over pooled nodes.
use super::{
    SAMPLE_TEXT_BYTES, SAMPLES,
    pool::{Node, TextRole},
    present::{Fit, Samples},
};
use crate::job::output::shape::COLLECTION_MEMBERS;

/// Bounds beyond which sample counts and text clips cannot change a presentation.
pub(super) struct Limits {
    depth: usize,
    last_sample: Option<usize>,
    sample_bytes: usize,
    whole_text: bool,
}

impl Limits {
    pub(super) fn of(node: &Node) -> Self {
        let mut limits = Self {
            depth: 0,
            last_sample: None,
            sample_bytes: 0,
            whole_text: true,
        };
        limits.depth = limits.visit(node);
        limits
    }

    fn visit(&mut self, node: &Node) -> usize {
        let mut depth = 0;
        match node {
            Node::Array { items, .. } => {
                for (index, node) in items {
                    self.last_sample = self.last_sample.max(Some(*index));
                    depth = depth.max(self.visit(node));
                }
            }
            Node::Object { members, total } => {
                for (index, _, node) in members {
                    if *total > COLLECTION_MEMBERS {
                        self.last_sample = self.last_sample.max(Some(*index));
                    }
                    depth = depth.max(self.visit(node));
                }
            }
            Node::Text(text) => {
                if text.role == TextRole::Sample {
                    self.sample_bytes = self.sample_bytes.max(text.prefix.len());
                    // Whole reads use each string's own limit, which can exclude
                    // the pooled lookahead byte. Do not equate those with a clip.
                    self.whole_text &= text.limit >= text.prefix.len();
                }
                return 0;
            }
            Node::Value(_) | Node::Complete(_) => return 0,
        }
        depth + 1
    }

    pub(super) fn candidates(&self, covered: bool) -> impl Iterator<Item = Fit> {
        let [clip, shorter @ ..] = SAMPLE_TEXT_BYTES;
        let deepest = |samples, clip| Fit {
            samples,
            clip: Some(clip),
            depth: None,
        };
        let every = covered
            .then_some([Fit::WHOLE, deepest(Samples::Every, clip)])
            .into_iter()
            .flatten();
        // Showing more samples can cost less: records of fully shown arrays, and
        // without any cut the shape, drop out. Every distinct count is tried.
        let counts = (1..=SAMPLES)
            .rev()
            .map(move |count| deepest(Samples::First(count), clip));
        let shorter = shorter
            .into_iter()
            .map(move |clip| deepest(Samples::First(1), clip));
        every
            .chain(counts)
            .chain(shorter)
            .chain((1..self.depth).rev().map(shallower))
    }

    pub(super) fn distinct(
        &self,
        candidates: impl Iterator<Item = Fit>,
    ) -> impl Iterator<Item = Fit> {
        let mut previous = None;
        candidates.filter(move |fit| {
            let mut key = *fit;
            // Pooled children keep source indices, including sparse/protected
            // ones. Their count is not a bound on the sample count that shows them.
            if let Samples::First(count) = key.samples
                && self.last_sample.is_none_or(|index| index < count)
            {
                key.samples = Samples::Every;
            }
            key.clip = match key.clip {
                Some(clip) if self.whole_text && clip >= self.sample_bytes => None,
                Some(clip) => Some(clip.min(self.sample_bytes)),
                None => None,
            };
            previous.replace(key) != Some(key)
        })
    }
}

pub(super) fn shallower(depth: usize) -> Fit {
    let [.., shortest] = SAMPLE_TEXT_BYTES;
    Fit {
        samples: Samples::First(1),
        clip: Some(shortest),
        depth: Some(depth),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Accounting, Pooled, pool::pool};
    use super::*;
    use crate::job::output::{FieldPointer, render::Clipped, shape::Shape};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    /// The reference deliberately presents every candidate, without pruning.
    /// Check either side of every emitted-size boundary, not just the usual budget.
    fn assert_equivalent(pooled: &Pooled, complete: &BTreeSet<FieldPointer>) {
        let field = FieldPointer::result();
        let limits = Limits::of(&pooled.node);
        let reference: Vec<_> = limits
            .candidates(pooled.covered)
            .chain(std::iter::once(shallower(0)))
            .map(|fit| pooled.attempt(&field, complete, fit))
            .collect();
        let budgets: BTreeSet<_> =
            [0, pooled.accounting.budget()]
                .into_iter()
                .chain(reference.iter().flat_map(|(_, bytes)| {
                    [bytes.saturating_sub(1), *bytes, bytes.saturating_add(1)]
                }))
                .collect();
        for budget in budgets {
            let (expected, _) = reference
                .iter()
                .find(|(_, bytes)| *bytes <= budget)
                .unwrap_or_else(|| reference.last().unwrap());
            let actual = pooled.fitted(&field, complete, budget);
            assert_eq!(actual.value, expected.value, "value at budget {budget}");
            assert_eq!(actual.shape, expected.shape, "shape at budget {budget}");
            assert_eq!(
                actual.truncated, expected.truncated,
                "cuts at budget {budget}"
            );
        }
    }

    fn both_paths(value: &Value, complete: &BTreeSet<FieldPointer>) {
        let field = FieldPointer::result();
        let bytes = serde_json::to_vec(value).unwrap();
        for accounting in [Accounting::Preview, Accounting::Page] {
            let memory = Pooled::new(
                Shape::of(value),
                Node::of(value.clone(), &field, complete, accounting),
                true,
                accounting,
            );
            assert_equivalent(&memory, complete);
            let mut reader = crate::job::output::json::reader(bytes.as_slice());
            let streamed = pool(
                &mut reader,
                &field,
                complete,
                &Clipped::new(),
                &Default::default(),
                accounting,
            )
            .unwrap();
            assert_equivalent(&streamed, complete);
        }
    }

    /// Pruned fitting chooses what the unpruned candidates would, including for
    /// collections without arrays, sparse protected samples, clipped text and
    /// whole reads that stop before the pooled lookahead byte.
    #[test]
    fn pruning_matches_the_unpruned_fitter() {
        let collection = |value: &Value| -> serde_json::Map<_, _> {
            (0..=COLLECTION_MEMBERS)
                .map(|index| (format!("key{index:03}"), value.clone()))
                .collect()
        };
        both_paths(
            &json!({"members": collection(&json!("v".repeat(40)))}),
            &BTreeSet::new(),
        );

        let complete = ["/result/complete".parse().unwrap()].into_iter().collect();
        for limit in SAMPLE_TEXT_BYTES {
            let prefix = "x".repeat(limit - 1);
            let value = json!({
                "items": [
                    format!("{prefix}🦀end"),
                    format!("{prefix}\r\nend"),
                    "x".repeat(limit),
                    "x".repeat(limit + 1),
                ],
                "field": "field\r\n".repeat(40),
                "complete": {"answer": "complete\r\n".repeat(40)},
            });
            both_paths(&value, &complete);
        }
        let inner: serde_json::Map<_, _> = (0..12)
            .map(|index| (format!("inner{index}"), json!(index)))
            .collect();
        let record: serde_json::Map<_, _> = (0..12)
            .map(|index| (format!("outer{index}"), json!(inner)))
            .collect();
        both_paths(&json!({"items": [record]}), &BTreeSet::new());

        let record = json!({"answer": "complete".repeat(40), "id": 1});
        for (key, contents, protected) in [
            ("array", json!(vec![record.clone(); 12]), "9"),
            ("members", json!(collection(&record)), "key009"),
        ] {
            let value = json!({(key): contents, "later": [0, 1, 2, 3, 4]});
            let complete = [FieldPointer::result()
                .property(key)
                .property(protected)
                .property("answer")]
            .into_iter()
            .collect();
            let mut node = Node::of(
                value.clone(),
                &FieldPointer::result(),
                &complete,
                Accounting::Preview,
            );
            let Node::Object { members, .. } = &mut node else {
                unreachable!()
            };
            // Keep an unprotected late sample and a still later complete field;
            // `later` tells losing index 7 apart from a shared count of 4.
            let (_, _, field) = members
                .iter_mut()
                .find(|(_, name, _)| name.as_str() == key)
                .unwrap();
            match field {
                Node::Array { items, .. } => items.retain(|(index, _)| [0, 7, 9].contains(index)),
                Node::Object { members, .. } => {
                    members.retain(|(index, _, _)| [0, 7, 9].contains(index));
                }
                _ => unreachable!(),
            }
            let pooled = Pooled::new(Shape::of(&value), node, false, Accounting::Preview);
            assert_equivalent(&pooled, &complete);
        }

        let [.., limit] = SAMPLE_TEXT_BYTES;
        let mut pooled = Pooled::of(json!("x".repeat(limit + 1)));
        let Node::Text(text) = &mut pooled.node else {
            unreachable!()
        };
        // A whole fit stops at this limit, while an explicit clip may include the
        // lookahead byte and drop the text cut.
        text.limit = limit;
        assert_equivalent(&pooled, &BTreeSet::new());
    }
}
