//! Merge cumulative counters without erasing late cached-token refinements.
use super::super::wire;
use super::Decoder;
use crate::provider::protocol::{ResponseChunk, Usage};

impl Decoder {
    /// Cumulative counters: missing or smaller late values keep the previous
    /// total, and cached tokens are clamped to the prompt.
    pub(super) fn update_usage(&mut self, usage: wire::Usage, chunks: &mut Vec<ResponseChunk>) {
        let prompt = usage
            .prompt_tokens
            .unwrap_or(self.raw_prompt_tokens)
            .max(self.raw_prompt_tokens);
        let output_tokens = usage
            .completion_tokens
            .unwrap_or(self.usage.output_tokens)
            .max(self.usage.output_tokens);
        let cached_input_tokens = usage
            .cached_tokens
            .unwrap_or(self.usage.cached_input_tokens)
            .max(self.usage.cached_input_tokens)
            .min(prompt);
        self.raw_prompt_tokens = prompt;
        self.usage = Usage {
            input_tokens: prompt - cached_input_tokens,
            cached_input_tokens,
            output_tokens,
        };
        chunks.push(ResponseChunk::UsageUpdated { usage: self.usage });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Decoder, tests::*};
    use crate::provider::protocol::{StopReason, Usage};
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
            packet(json!({"cached_tokens":80}), 3),
            packet(Value::Null, 4),
            packet(json!({"cached_tokens":null}), 5),
            end("stop"),
        ]);
        let expected = Usage {
            output_tokens: 5,
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
                let mut decoder = Decoder::new("test-model".into());
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
                assert_eq!(decoder.usage, USAGE, "stage {stage}: {usage}");
            }
        }
        // Cached tokens never exceed the prompt.
        let mut decoder = Decoder::new("test-model".into());
        let packet = json!({"usage":{"prompt_tokens":10,"completion_tokens":1,
            "prompt_tokens_details":{"cached_tokens":50}}});
        decoder.decode(&event(packet)).unwrap();
        assert_eq!(decoder.usage.cached_input_tokens, 10);
        assert_eq!(decoder.usage.input_tokens, 0);
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
        let (_, usage, stop) = decode(vec![
            delta(json!({"content":"partial"})),
            end("length"),
            packet(9, Value::Null),
            packet(10, json!(80)),
            event(json!({"choices":[{"finish_reason":"length","delta":null}]})),
        ]);
        assert_eq!((stop, usage), (StopReason::MaxTokens, USAGE));
    }
}
