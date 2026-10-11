//! Token accounting for usage reports. A family's spelling locates the
//! counters, its accounting says what they mean, and the fold turns them into
//! Skyhook's counters without losing late refinements.

use serde_json::Value;

use crate::provider::{codec::common::lenient_u64, protocol::Usage};

/// What a family's input counter includes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputAccounting {
    /// `input` is the whole prompt; cache reads and writes are parts of it.
    PromptTotal,
    /// `input` is only the prompt's uncached part; cache reads and writes are
    /// reported beside it.
    FreshPrompt,
}

/// Counters as one usage report spells them; a missing counter keeps the
/// earlier value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Observed {
    pub input: Option<u64>,
    pub cached: Option<u64>,
    pub written: Option<u64>,
    pub output: Option<u64>,
}

/// Where a family's usage report spells each counter: JSON pointers, of which
/// the first holding a counter wins.
pub(crate) struct Spelling {
    pub input: &'static [&'static str],
    pub cached: &'static [&'static str],
    pub written: &'static [&'static str],
    pub output: &'static [&'static str],
}

impl Observed {
    /// The counters `usage` reports, which some servers encode as floats or
    /// strings; `None` when it reports none, or only zeros: a backend's way of
    /// reporting nothing.
    pub(crate) fn read(usage: &Value, spelling: &Spelling) -> Option<Self> {
        let counter = |pointers: &[&str]| {
            pointers
                .iter()
                .find_map(|pointer| usage.pointer(pointer).and_then(lenient_u64))
        };
        let observed = Self {
            input: counter(spelling.input),
            cached: counter(spelling.cached),
            written: counter(spelling.written),
            output: counter(spelling.output),
        };
        let counts = [
            observed.input,
            observed.cached,
            observed.written,
            observed.output,
        ];
        counts
            .into_iter()
            .flatten()
            .any(|count| count > 0)
            .then_some(observed)
    }
}

/// Cumulative counters of one response. Reports never regress a counter, so a
/// late cache breakdown refines the split while the total stands.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Counters {
    accounting: InputAccounting,
    seen: Observed,
}

impl Counters {
    pub(crate) fn new(accounting: InputAccounting) -> Self {
        Self {
            accounting,
            seen: Observed::default(),
        }
    }

    pub(crate) fn observe(&mut self, observed: Observed) -> Usage {
        let latest = |current: &mut Option<u64>, value: Option<u64>| {
            *current = match (*current, value) {
                (Some(current), Some(value)) => Some(current.max(value)),
                (current, value) => current.or(value),
            };
        };
        latest(&mut self.seen.input, observed.input);
        latest(&mut self.seen.cached, observed.cached);
        latest(&mut self.seen.written, observed.written);
        latest(&mut self.seen.output, observed.output);
        self.usage()
    }

    /// Skyhook's counts, of which only those the reports determine are present.
    pub(crate) fn usage(&self) -> Usage {
        let Observed {
            input,
            cached,
            written,
            output,
        } = self.seen;
        match self.accounting {
            // Cache reads and writes are parts of the total: without it the uncached
            // part is unknown, and with it they are clamped to it.
            InputAccounting::PromptTotal => {
                let cached = match input {
                    Some(total) => cached.map(|cached| cached.min(total)),
                    None => cached,
                };
                let input = input.map(|total| total - cached.unwrap_or_default());
                let written = match input {
                    Some(input) => written.map(|written| written.min(input)),
                    None => written,
                };
                Usage {
                    input_tokens: input,
                    cached_input_tokens: cached,
                    cache_write_input_tokens: written,
                    output_tokens: output,
                }
            }
            // Cache creation is part of the uncached input, which only the fresh
            // count determines.
            InputAccounting::FreshPrompt => Usage {
                input_tokens: input.map(|input| input.saturating_add(written.unwrap_or_default())),
                cached_input_tokens: cached,
                cache_write_input_tokens: written,
                output_tokens: output,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unusable_counters_read_as_absent() {
        const SPELLING: Spelling = Spelling {
            input: &["/input", "/alias"],
            cached: &["/cached"],
            written: &["/written"],
            output: &["/output"],
        };
        let read = |usage: Value| Observed::read(&usage, &SPELLING);
        // An unusable counter falls through to the next spelling, or is absent.
        assert_eq!(
            read(
                serde_json::json!({"input":"garbage","alias":"12","cached":-1,
                "written":1.5,"output":7.0})
            ),
            Some(Observed {
                input: Some(12),
                output: Some(7),
                ..Default::default()
            })
        );
        assert_eq!(
            read(serde_json::json!({"input":"garbage","cached":null,"output":{}})),
            None
        );
        // Zeros alone report nothing; a zero beside a real count is one.
        assert_eq!(
            read(serde_json::json!({"input":0,"cached":0,"output":0})),
            None
        );
        assert_eq!(
            read(serde_json::json!({"input":5,"cached":0})),
            Some(Observed {
                input: Some(5),
                cached: Some(0),
                ..Default::default()
            })
        );
    }

    #[test]
    fn total_accounting_refines_the_split_without_moving_the_total() {
        let mut counters = Counters::new(InputAccounting::PromptTotal);
        // Cache reads alone measure no prompt: its uncached part is unknown.
        let cached_only = counters.observe(Observed {
            cached: Some(80),
            output: Some(1),
            ..Default::default()
        });
        assert_eq!(
            (cached_only.input_tokens, cached_only.occupancy()),
            (None, None)
        );
        let refined = counters.observe(Observed {
            input: Some(100),
            written: Some(30),
            output: Some(3),
            ..Default::default()
        });
        assert_eq!(refined, Usage::prompted(20, 80, 20, 3));
        // Regressions and gaps keep the earlier counters; cached is clamped to the prompt.
        let held = counters.observe(Observed {
            input: Some(90),
            cached: Some(300),
            ..Default::default()
        });
        assert_eq!(held, Usage::prompted(0, 100, 0, 3));
    }

    #[test]
    fn fresh_accounting_sums_exclusive_counters() {
        let mut counters = Counters::new(InputAccounting::FreshPrompt);
        // Cache creation alone leaves the uncached input unknown.
        let created = counters.observe(Observed {
            written: Some(13),
            output: Some(1),
            ..Default::default()
        });
        assert_eq!((created.input_tokens, created.occupancy()), (None, None));
        let usage = counters.observe(Observed {
            input: Some(11),
            cached: Some(17),
            ..Default::default()
        });
        assert_eq!(usage, Usage::prompted(24, 17, 13, 1));
    }
}
