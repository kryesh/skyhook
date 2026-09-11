//! Validate cumulative counters without erasing late cached-token refinements.
use super::super::wire;
use super::Decoder;
use crate::provider::{
    ProviderError,
    protocol::{ResponseChunk, Usage},
};

impl Decoder {
    pub(super) fn update_usage(
        &mut self,
        usage: wire::Usage,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        // Prompt totals are monotone, but uncached input may fall when a
        // later packet refines the cached-token breakdown.
        let prompt = usage.prompt_tokens;
        let usage = decode_usage(usage, self.usage.cached_input_tokens)?;
        if prompt < self.raw_prompt_tokens
            || usage.cached_input_tokens < self.usage.cached_input_tokens
            || usage.output_tokens < self.usage.output_tokens
        {
            return Err(ProviderError::protocol("Chat usage counters regressed"));
        }
        self.raw_prompt_tokens = prompt;
        self.usage = usage;
        chunks.push(ResponseChunk::UsageUpdated { usage });
        Ok(())
    }
}

fn decode_usage(value: wire::Usage, previous_cached: u64) -> Result<Usage, ProviderError> {
    let input_tokens = value.prompt_tokens;
    let output_tokens = value.completion_tokens;
    if let Some(total) = value.total_tokens
        && input_tokens.checked_add(output_tokens) != Some(total)
    {
        return Err(ProviderError::protocol(
            "Chat usage total_tokens does not match prompt + completion",
        ));
    }
    // Missing/null cache details do not erase a previously reported breakdown.
    let cached_input_tokens = value
        .prompt_tokens_details
        .and_then(|details| details.cached_tokens)
        .unwrap_or(previous_cached);
    if cached_input_tokens > input_tokens {
        return Err(ProviderError::protocol(
            "Chat cached_tokens exceeds prompt_tokens",
        ));
    }
    Ok(Usage {
        input_tokens: input_tokens - cached_input_tokens,
        cached_input_tokens,
        output_tokens,
    })
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
        assert_eq!(
            usage,
            Usage {
                input_tokens: 20,
                cached_input_tokens: 80,
                output_tokens: 5
            }
        );
    }

    #[test]
    fn invalid_usage_is_rejected_during_generation_after_finish_and_on_repeated_finish() {
        for usage in [
            json!({}),
            json!({"prompt_tokens":100}),
            json!({"prompt_tokens":"100","completion_tokens":10}),
            json!({"prompt_tokens":100,"completion_tokens":-1}),
            json!({"prompt_tokens":100,"completion_tokens":10,"total_tokens":109}),
            json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":101}}),
            json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":-1}}),
            json!({"prompt_tokens":99,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":80}}),
            json!({"prompt_tokens":100,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":80}}),
            json!({"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":79}}),
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
                assert!(
                    decoder.decode(&event(packet)).is_err(),
                    "stage {stage}: {usage}"
                );
                assert!(decoder.finish().is_err());
            }
        }
    }

    #[test]
    fn repeated_finish_can_refine_usage() {
        let packet = |prompt, completion, cached| {
            json!({
                "choices":[{"finish_reason":"length"}],
                "usage":{
                    "prompt_tokens":prompt,"completion_tokens":completion,
                    "prompt_tokens_details":{"cached_tokens":cached}
                }
            })
        };
        let initial = packet(100, 9, Value::Null);
        let refined = packet(100, 10, json!(80));
        let (_, usage, stop) = decode(vec![
            delta(json!({"content":"partial"})),
            end("length"),
            event(initial.clone()),
            event(refined.clone()),
            event(json!({"choices":[{"finish_reason":"length","delta":null}]})),
        ]);
        assert_eq!(stop, StopReason::MaxTokens);
        assert_eq!(
            usage,
            Usage {
                input_tokens: 20,
                cached_input_tokens: 80,
                output_tokens: 10
            }
        );
    }
}
