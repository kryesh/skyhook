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
    /// strings; `None` when it reports none.
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
        (observed != Self::default()).then_some(observed)
    }
}

/// Cumulative counters of one response. Reports never regress a counter, so a
/// late cache breakdown refines the split while the total stands.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Counters {
    accounting: InputAccounting,
    input: u64,
    cached: u64,
    written: u64,
    output: u64,
}

impl Counters {
    pub(crate) fn new(accounting: InputAccounting) -> Self {
        Self {
            accounting,
            input: 0,
            cached: 0,
            written: 0,
            output: 0,
        }
    }

    pub(crate) fn observe(&mut self, observed: Observed) -> Usage {
        let latest = |current: u64, value: Option<u64>| value.unwrap_or(current).max(current);
        self.input = latest(self.input, observed.input);
        self.cached = latest(self.cached, observed.cached);
        self.written = latest(self.written, observed.written);
        self.output = latest(self.output, observed.output);
        self.usage()
    }

    pub(crate) fn usage(&self) -> Usage {
        match self.accounting {
            InputAccounting::PromptTotal => {
                let cached = self.cached.min(self.input);
                let input = self.input - cached;
                Usage {
                    input_tokens: input,
                    cached_input_tokens: cached,
                    cache_write_input_tokens: self.written.min(input),
                    output_tokens: self.output,
                }
            }
            InputAccounting::FreshPrompt => Usage {
                input_tokens: self.input.saturating_add(self.written),
                cached_input_tokens: self.cached,
                cache_write_input_tokens: self.written,
                output_tokens: self.output,
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
    }

    #[test]
    fn total_accounting_refines_the_split_without_moving_the_total() {
        let mut counters = Counters::new(InputAccounting::PromptTotal);
        let first = counters.observe(Observed {
            input: Some(100),
            output: Some(1),
            ..Default::default()
        });
        assert_eq!((first.input_tokens, first.cached_input_tokens), (100, 0));
        let refined = counters.observe(Observed {
            cached: Some(80),
            written: Some(30),
            output: Some(3),
            ..Default::default()
        });
        assert_eq!(
            (
                refined.input_tokens,
                refined.cached_input_tokens,
                refined.cache_write_input_tokens,
                refined.output_tokens
            ),
            (20, 80, 20, 3)
        );
        // Regressions and gaps keep the earlier counters; cached is clamped to the prompt.
        let held = counters.observe(Observed {
            input: Some(90),
            cached: Some(300),
            ..Default::default()
        });
        assert_eq!((held.input_tokens, held.cached_input_tokens), (0, 100));
    }

    #[test]
    fn fresh_accounting_sums_exclusive_counters() {
        let mut counters = Counters::new(InputAccounting::FreshPrompt);
        let usage = counters.observe(Observed {
            input: Some(11),
            written: Some(13),
            cached: Some(17),
            output: Some(1),
        });
        assert_eq!(
            (
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.cache_write_input_tokens
            ),
            (24, 17, 13)
        );
    }
}
