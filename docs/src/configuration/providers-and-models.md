# Providers and models

A provider entry names an endpoint and the models served through it, under `models`. Everywhere a
model is chosen or recorded—`--model`, the `/model` picker, the `agent` tool, `default_model`,
session history and `skyhook stats`—it is named `provider/model` from the two keys, so neither key
may contain `/`. The `model` field inside a profile is the identifier sent on the wire and stays
free-form. `default_model` selects the model a new session starts with when neither `--model` nor
the model last submitted in the workspace chooses one; without it, the first model of the first
provider is used.

Every entry names a **dialect** and a **codec**. The codec is the API family the endpoint speaks:
`chat_completions`, `responses`, or `messages`; `base_url` names the API root and Skyhook appends
`/chat/completions`, `/responses`, or `/messages`. The dialect is the set of wire conventions the
server follows:

| Dialect | Codecs | Use for |
| --- | --- | --- |
| `compatible` | any | Standards-following servers: OpenAI- or Anthropic-compatible endpoints need only `base_url`. A key is sent as each API specifies: a bearer token, or `x-api-key` on `messages`. |
| `openai` | `chat_completions`, `responses` | `https://api.openai.com/v1`. Response schemas must fit OpenAI's strict subset, prompt caching is keyed per conversation, and Chat keeps reasoning local because the API accepts none back. Extra fields: `reasoning_summary: false` for an unverified organisation (Responses only), `organization`, `project`. |
| `anthropic` | `messages` | `https://api.anthropic.com/v1`. Extra fields: `workspace_id`, `cache_ttl: "5m"` or `"1h"`. |
| `codex` | `responses` | The ChatGPT subscription service; no `api_key`—run `skyhook auth login`. `base_url` defaults to `https://chatgpt.com/backend-api/codex` and the extra field `auth_url` to `https://auth.openai.com`; set them only for a mirror. |
| `openrouter` | any | `https://openrouter.ai/api/v1`. Reasoning effort and returned reasoning follow OpenRouter's conventions, history is marked for prompt caching, and each conversation keeps its routing affinity. Extra fields: `routing` (Chat only: `order`, `allow_fallbacks`, `require_parameters`, `data_collection: "allow"` or `"deny"`, `quantizations`, `zdr`, `fallback_models`) and `cache_ttl: "5m"` or `"1h"`. A response schema always sets `require_parameters`. |
| `litellm` | any | A LiteLLM proxy; `api_key` is the virtual key. Required `upstream: "openai"`, `"anthropic"` or `"bedrock"` names the family behind the alias, which the proxy hides: Claude on Chat keeps its signed thinking and prompt caching, OpenAI on Chat has neither; `messages` behind an OpenAI upstream goes through the proxy's translation, which drops reasoning. Optional `tags` select LiteLLM's tag-based routing and label its spend logs; each conversation is reported as one LiteLLM session. |

Every dialect but `codex` accepts `api_key` (see [authentication](authentication.md)); every
dialect accepts fixed request `headers` for proxies or attribution, the
[connection timeouts](#connection-timeouts), and `models`. On `messages`, a configured
`anthropic-beta` adds to the betas Skyhook announces rather than replacing them. The credential
header and header placements (`cache_key: {header: …}`, or a dialect's own) in turn replace an
entry header of the same name, whose command then never runs; so does `Accept`, which is always
`text/event-stream`. Unknown fields, and a codec the dialect does not speak, are rejected. There
are no model aliases or automatic vendor detection: `model` is the identifier sent on the wire.

A `compatible` entry may place each convention itself when a server deviates, and any model may
carry the same selections under `overrides` to differ from its provider:

```yaml
providers:
  local:
    codec: "chat_completions"
    dialect: "compatible"
    base_url: "http://127.0.0.1:8000/v1"
    output_limit:                                # or: omitted
      field: "max_tokens"
    reasoning_replay:                            # the assistant-message key; or: omitted
      field: "reasoning"
    cache_key:                                   # or `header: "x-session-id"`, or: omitted
      body: "cache_salt"
    models:
      thinker:
        model: "served-name"
        max_context: 131072
        max_output: 32768
        overrides:
          reasoning_effort: "reasoning_effort"
```

The selectable dimensions are `output_limit`, `reasoning_effort` (a field path), `reasoning_replay`
and `tool_stream` (Chat Completions only), `cache_key`, and `user_id`. A field path is
dot-separated (`metadata.user_id`); `cache_key` and `user_id` carry the conversation identity.
Paths must not name a field the codec writes itself (such as `model` or `messages`), and no two may
overlap, including one inside another (`reasoning` and `reasoning.effort`). A `cache_key` header
must not be `accept`, `content-type`, or a header the codec sends itself (such as
`anthropic-version` on `messages`). A `reasoning_replay`
field moves the provider's replay to another key in the same form. Messages requires an output
limit, so `output_limit: omitted` is refused there.

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

| Model | Context | Output | Source |
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
  --n-predict 131072
```

Then configure Skyhook to match:

```yaml
approve_all: false

providers:
  local:
    codec: "chat_completions"
    dialect: "compatible"
    base_url: "http://127.0.0.1:8080/v1"
    models:
      qwen:
        model: "qwen3.8-27b"
        max_context: 262144
        max_output: 131072
        supports_images: false
```

Select it as `local/qwen`.

Here **131,072 is a chosen total-generation cap**, matching `--n-predict`, not a published
hard output limit of Qwen3.8-27B. Qwen recommends separate reasoning and final-response budgets
for some extended-context deployments; this example instead shares one allowance between
reasoning and final text. The complete prompt and generated output must still fit the context.

The `model` value matches the server's `--alias`. Current llama.cpp builds render chat templates
and split reasoning into `reasoning_content` by default, matching Skyhook's default replay
convention. The command is text-only, so `supports_images` stays false; image input needs a
compatible multimodal projector and a tested server configuration.

Verify the server's allocated context in its startup logs. If memory requires a smaller context,
reduce both the server setting and Skyhook's budget; with multiple slots, use the actual per-slot
capacity, not an assumed aggregate. See the
[llama.cpp server documentation](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)
for server options.

## Model failure recovery

For every provider, **transient/network failures retry until success or cancellation**,
with no attempt limit. This includes connection failures, interrupted streams, timeouts,
rate limits, temporary server errors, and an expired command-sourced credential
([authentication](authentication.md#api-keys-and-environment-files)). Other authentication,
billing (exhausted quota, credit, or spend limit, however the service reports it),
invalid-request, and malformed-response errors stop the request instead of retrying. So does a
response with neither visible text nor a tool call, such as reasoning alone; cut off at
`max_output`, it reports that the output limit was reached.

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
provider, endpoint, API, and model. Switching to an incompatible model omits that reasoning from
requests without deleting the saved history. Signed thinking cannot be reused across compaction
or mode switches. Visible summaries cannot replace private reasoning state the service requires.
Messages servers that do not sign thinking still get it replayed, until a signed block appears in
the context; from then on only signed blocks are sent.

Standard Chat Completions has no portable request-side reasoning field. Compatible Chat providers
replay their returned reasoning under **`reasoning_content` by default**. Ordinary Chat responses
without reasoning do not acquire an invented reasoning field.
A server that spells the key differently selects it with `reasoning_replay` set to
`field: "reasoning"`; `reasoning_replay: omitted` keeps reasoning local. The `openai` dialect never
replays on Chat, since the official API rejects unknown message keys; `litellm` with a Claude
upstream replays the signed thinking blocks the proxy returns, and `openrouter` replays its
`reasoning_details`; both are bound to the exact conversation like Messages thinking. As on
Messages, unsigned reasoning is replayed until signed reasoning appears in the context. From then
on, only signed thinking blocks are sent, and a turn's `reasoning_details` are sent whole, in
order, only if one of them is signed. Responses and Messages replay native reasoning automatically.

Reasoning is replayed with its owning assistant turn, including turns that only call tools;
it is never merged into answer text or fabricated `<think>` tags. Server-side tool and reasoning
parsers/templates must be configured appropriately; Skyhook does not infer them from model names.
Chat requests send `max_output` as `max_completion_tokens`; a server that only honours
`max_tokens` sets `output_limit` to `field: "max_tokens"`.

## Connection timeouts

Every provider accepts positive `startup_timeout_secs` and `read_idle_timeout_secs` settings
(both default to 600 seconds). Startup is a deadline for each HTTP attempt, while read-idle resets
after each response-body chunk. For example:

```yaml
providers:
  local:
    codec: "chat_completions"
    dialect: "compatible"
    base_url: "http://127.0.0.1:8080/v1"
    startup_timeout_secs: 600
    read_idle_timeout_secs: 600
```

Each retry gets fresh deadlines, so these settings do not limit the total time spent on a
response. Cancel the request to stop waiting; the server may continue processing if it does not
honor disconnects. See [model failure recovery](#model-failure-recovery) for retry behavior.

`skyhook dump config` writes the resolved timeouts, so a dump shows the defaults an entry left
implicit.

See [authentication](authentication.md) for API-key and Codex login setup,
and [sessions and context](../guide/sessions-and-context.md#conversation-compaction) for
context budgeting and compaction.
