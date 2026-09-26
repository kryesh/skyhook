//! Report cumulative counters through the shared fold, keeping late refinements.
use super::super::wire;
use super::Decoder;
use crate::provider::{codec::usage::Observed, protocol::ResponseEvent};

impl Decoder {
    pub(super) fn update_usage(&mut self, usage: wire::Usage, events: &mut Vec<ResponseEvent>) {
        let usage = self.usage.observe(Observed {
            input: usage.prompt_tokens,
            cached: usage.cached_tokens,
            written: usage.cache_write_tokens,
            output: usage.completion_tokens,
        });
        events.push(ResponseEvent::Usage(usage));
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::provider::protocol::Usage;
    use serde_json::{Value, json};

    #[test]
    fn late_cache_refinement_and_null_cache_details_preserve_totals() {
        let packet = |details: Value, output| {
            event(json!({"choices":[],"usage":{
                "prompt_tokens":100,"completion_tokens":output,"prompt_tokens_details":details
            }}))
        };
        let (_, usage, _) = decode(vec![
            delta(json!({"content":"answer"})),
            event(json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":1}})),
            packet(json!({"cached_tokens":null}), 2),
            packet(json!({"cached_tokens":80, "cache_write_tokens":5}), 3),
            packet(Value::Null, 4),
            packet(json!({"cached_tokens":null}), 5),
            end("stop"),
        ]);
        let expected = Usage {
            output_tokens: 5,
            cache_write_input_tokens: 5,
            ..USAGE
        };
        assert_eq!(usage, expected);
    }

    #[test]
    fn loose_usage_is_merged_monotonically_at_every_stage() {
        for usage in [
            // Mismatched totals, string counters, and alternate key names.
            json!({"prompt_tokens":100,"completion_tokens":10,"total_tokens":1}),
            json!({"prompt_tokens":"100","completion_tokens":10.0}),
            json!({"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":80}),
            // Regressions and missing counters keep the previous totals.
            json!({"prompt_tokens":99,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":79}}),
            json!({"prompt_tokens":100}),
            json!({"completion_tokens":-1, "prompt_tokens":100}),
        ] {
            for stage in 0..3 {
                let mut decoder = decoder();
                let mut packet = phantom_usage_chunk();
                packet["choices"] = json!([]);
                decoder.decode(&event(packet.clone())).unwrap();
                if stage > 0 {
                    decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                    decoder.decode(&end("length")).unwrap();
                    packet["choices"] = json!([{"index":0,"delta":{}}]);
                }
                if stage == 2 {
                    packet["choices"][0]["finish_reason"] = json!("length");
                }
                packet["usage"] = usage.clone();
                decoder.decode(&event(packet)).unwrap();
                assert_eq!(decoder.usage.usage(), USAGE, "stage {stage}: {usage}");
            }
        }
        // Cached tokens never exceed the prompt.
        let mut decoder = decoder();
        let packet = json!({"usage":{"prompt_tokens":10,"completion_tokens":1,
            "prompt_tokens_details":{"cached_tokens":50}}});
        decoder.decode(&event(packet)).unwrap();
        assert_eq!(decoder.usage.usage().cached_input_tokens, 10);
        assert_eq!(decoder.usage.usage().input_tokens, 0);
    }

    #[test]
    fn repeated_finish_can_refine_usage() {
        let packet = |completion, cached: Value| {
            event(json!({
                "choices":[{"finish_reason":"length"}],
                "usage":{"prompt_tokens":100,"completion_tokens":completion,
                    "prompt_tokens_details":{"cached_tokens":cached}}
            }))
        };
        let (_, usage, outcome) = decode(vec![
            delta(json!({"content":"partial"})),
            end("length"),
            packet(9, Value::Null),
            packet(10, json!(80)),
            event(json!({"choices":[{"finish_reason":"length","delta":null}]})),
        ]);
        assert_eq!((outcome, usage), (MAX_TOKENS, USAGE));
    }
}
