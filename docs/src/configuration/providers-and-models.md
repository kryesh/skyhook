# Providers and models

Providers and models are separate named profiles. API secrets can be read from environment variables
or retrieved lazily by a command; no literal API-key field is supported in YAML. `openai` requires
an explicit `base_url` and `api` (`chat_completions` or `responses`); `anthropic` requires an explicit
`base_url`. Both accept either `api_key_env` or `api_key_command`, or neither for a keyless endpoint.
URLs name the API root: Skyhook appends
`/chat/completions`, `/responses`, or `/messages`. For the official services use
`https://api.openai.com/v1` or `https://api.anthropic.com/v1`. There are no vendor presets, model
aliases, or automatic vendor detection. Use full model identifiers.

## Context and output budgets

Every model profile requires `max_context` and `max_output`. Both must be positive, and
`max_output` must be smaller than `max_context`. Prompt/history, reasoning, and generated
text share the context window. Automatic [compaction](../guide/sessions-and-context.md#conversation-compaction)
uses the completed response's reported token usage at 80% of `max_context`, independently
of `max_output`; it does not estimate the next request.
Use the model's published limits or your server's actual configured limits, whichever is lower.
You can deliberately choose smaller budgets for cost, latency, or available memory.

`state_mode` controls how per-request runtime state (date, active jobs, and todos) reaches the model.
`dynamic` (the default) sends a fresh snapshot after the history on each request without appending
it to the conversation.
`persist` keeps each snapshot in the conversation. Use it for models that bind signed reasoning
to the exact earlier conversation (such as Claude Fable 5.1), at the cost of keeping every snapshot in context until
compaction. `none` sends no runtime state. See [runtime state](../reference/runtime-state.md)
for what the model receives.

The [complete example](overview.md#complete-example) uses published hosted-model limits:

| Profile | Context | Output | Source |
| --- | ---: | ---: | --- |
| GPT-5.6 / GPT-5.6 Sol, OpenAI API | 1,050,000 | 128,000 | [Model documentation](https://developers.openai.com/api/docs/models/gpt-5.6-sol) |
| Claude Sonnet 4.6 | 1,000,000 | 128,000 | [Model documentation](https://platform.claude.com/docs/en/models/sonnet-4-6/overview) |
| GPT-5.6 Sol, Codex subscription | 872,000 | 128,000 (not enforced) | [Codex model catalog](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) |

The Codex catalog advertises a default context of 272,000 and a maximum configurable context
of 872,000, distinct from the public API's model limit. The example uses that catalog maximum.
Codex subscription does not accept an output-token limit. Its `max_output`
profile field remains required and records GPT-5.6 Sol's published output limit, but it neither
enforces an endpoint limit nor sets the compaction threshold. Actual endpoint limits remain
authoritative.

## Qwen3.8-27B with llama.cpp

[Qwen's model card](https://huggingface.co/Qwen/Qwen3.8-27B) specifies **262,144 native context
tokens**. Its extended-context claims do not mean every local deployment allocates that capacity.
Use a compatible llama.cpp build and GGUF, and configure one server slot explicitly:

```sh
llama-server \
  --model /path/to/Qwen3.8-27B.gguf \
  --alias qwen3.8-27b \
  --host 127.0.0.1 --port 8080 \
  --ctx-size 262144 --parallel 1 \
  --n-predict 131072 \
  --jinja --reasoning-format deepseek
```

Then configure Skyhook to match:

```yaml
approve_all: false

providers:
  local:
    kind: "openai"
    base_url: "http://127.0.0.1:8080/v1"
    api: "chat_completions"

models:
  local:
    provider: "local"
    model: "qwen3.8-27b"
    max_context: 262144
    max_output: 131072
    supports_images: false
```

Here **131,072 is a chosen total-generation cap**, matching `--n-predict`, not a published
hard output limit of Qwen3.8-27B. Qwen recommends separate reasoning and final-response budgets
for some extended-context deployments; this example instead shares one allowance between
reasoning and final text. The complete prompt and generated output must still fit the context.

The `model` value matches the server's `--alias`. `--reasoning-format deepseek` returns
reasoning in `reasoning_content`, matching Skyhook's default replay convention. The command
is text-only, so `supports_images` stays false; image input needs a compatible multimodal
projector and a tested server configuration.

Verify the server's allocated context in its startup logs. If memory requires a smaller context,
reduce both the server setting and Skyhook's budget; with multiple slots, use the actual per-slot
capacity, not an assumed aggregate. See the
[llama.cpp server documentation](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)
for server options.

## Model failure recovery

For every provider, **transient/network failures retry until success or cancellation**,
with no attempt limit. This includes connection failures, interrupted streams, timeouts,
rate limits, and temporary server errors. Authentication, invalid-request, and malformed-response
errors stop the request instead of retrying.

A valid server `Retry-After` hint—delay-seconds or an HTTP-date—sets the delay,
even when it exceeds 30 seconds. Past dates mean no delay; malformed or ambiguous
headers fall back to the default backoff.
Without a valid hint, delays grow exponentially: **1s, 2s, 4s, 8s, 16s, 30s**, then
remain at 30s. The backoff restarts when a new request is prepared, including after compaction.
Cancellation interrupts the wait.

Retries resend the same request: queued input and notifications wait until that response
completes. Completed tools are not rerun, and partial tool calls from failed attempts are never
executed. The failed partial response is discarded. The interface and session records may show
retry status, but agents receive only the successful response or a terminal failure.

Context-window overflow triggers [compaction](../guide/sessions-and-context.md#conversation-compaction)
rather than resending an oversized request indefinitely. Recovery stops after three context
failures for one response; an invalid summary also has a separate three-attempt limit.
Transient failures during summarization follow the same unlimited retry policy and do not
consume either limit.

This prevents duplicate **Skyhook tool execution**, not duplicate provider inference: the
provider may have processed the interrupted request, and additional usage may be incurred.
Usage reported before interruption is retained; unreported provider usage remains unknown.
Retries also cannot guarantee that tools hosted by the provider will not repeat side effects.

## Reasoning history and local-server compatibility

Responses and Codex automatically request reasoning summaries, even when no model `reasoning`
effort is configured. Anthropic requests summarized thinking when `reasoning` enables thinking;
with `reasoning` unset, the model's default applies and may omit thinking text.
Skyhook displays only the reasoning text or summaries the service returns, not otherwise-private
reasoning. Support varies by provider, model, and compatible server.

Skyhook saves returned reasoning and automatically reuses it when compatible with the selected
provider, endpoint, API, and model. Switching to an incompatible profile omits that reasoning from
requests without deleting the saved history. Signed thinking cannot be reused across compaction
or mode switches. Visible summaries cannot replace private reasoning state the service requires.

Standard Chat Completions has no portable request-side reasoning field. Compatible Chat providers
replay their returned reasoning using **`reasoning_content` by default**: this is consumed by
llama.cpp and SGLang, and accepted as an alias by current vLLM. Ordinary Chat responses without
reasoning do not acquire an invented reasoning field. Configure a different spelling or disable
request replay on the **provider**, not individual model profiles:

```yaml
providers:
  local:
    kind: "openai"
    base_url: "http://127.0.0.1:8080/v1"
    api: "chat_completions"
    # Optional; this is the default:
    chat_reasoning_replay: "reasoning_content"
    # Alternatives: "reasoning", or "unsupported" to keep reasoning locally only.
```

This option belongs only to OpenAI-compatible Chat Completions. Anthropic and Codex reject it as an
unknown setting; configuring it with `api: "responses"` is also rejected. Their native reasoning
replay remains automatic and independent of this option. All models using a provider share its
Chat convention; use separate provider entries if a proxy routes to incompatible conventions.

Reasoning is replayed with its owning assistant turn, including tool calls and reasoning-only turns;
it is never merged into answer text or fabricated `<think>` tags. Server-side tool and reasoning
parsers/templates must be configured appropriately; Skyhook does not infer them from model names.
Chat requests send `max_output` as `max_completion_tokens` without renaming, so a server that only
honours `max_tokens` ignores the limit.

## Connection timeouts

OpenAI-compatible and Anthropic providers also accept positive `startup_timeout_secs` and
`read_idle_timeout_secs` settings (both default to 600 seconds). Startup is a deadline for each HTTP
attempt, while read-idle resets after each response-body chunk. For example:

```yaml
providers:
  local:
    kind: "openai"
    base_url: "http://127.0.0.1:8080/v1"
    api: "chat_completions"
    startup_timeout_secs: 600
    read_idle_timeout_secs: 600
```

Each retry gets fresh deadlines, so these settings do not limit the total time spent on a
response. Cancel the request to stop waiting; the server may continue processing if it does not
honor disconnects. See [model failure recovery](#model-failure-recovery) for retry behavior.

`skyhook dump config` writes the resolved timeouts and, for Chat Completions, the resolved
`chat_reasoning_replay`, so a dump shows the defaults an entry left implicit.

See [authentication](authentication.md) for API-key and Codex login setup,
and [sessions and context](../guide/sessions-and-context.md#conversation-compaction) for
context budgeting and compaction.
