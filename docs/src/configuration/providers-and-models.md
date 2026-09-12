# Providers and models

Providers and models are separate named profiles. API secrets can be read from environment variables
or retrieved lazily by a command; no literal API-key field is supported in TOML. `openai` requires
an explicit `base_url` and `api` (`chat_completions` or `responses`); `anthropic` requires an explicit
`base_url`. Both accept either `api_key_env` or `api_key_command`, or neither for a keyless endpoint.
URLs name the API root: Skyhook appends
`/chat/completions`, `/responses`, or `/messages`. For the official services use
`https://api.openai.com/v1` or `https://api.anthropic.com/v1`. There are no vendor presets, model
aliases, or automatic vendor detection. Use full model identifiers. A shared Chat codec normalizes
specific compatible streaming variations; backend wire types and replay policies stay behind the
unified `Provider` / `ProviderContext` interface.

Every model profile requires `max_context` and `max_output`. Both must be positive, and
`max_output` must be smaller than `max_context`. Prompt/history, reasoning, and generated
text share the context window. Automatic [compaction](../guide/sessions-and-context.md#conversation-compaction)
uses the completed response's reported token usage at 80% of `max_context`, independently
of `max_output`; it does not estimate the next request.
Use the model's published limits or your server's actual configured limits, whichever is lower.
You can deliberately choose smaller budgets for cost, latency, or available memory.

The [complete example](overview.md#complete-example) uses published hosted-model limits:

| Profile | Context | Output | Source |
| --- | ---: | ---: | --- |
| GPT-5.6 / GPT-5.6 Sol, OpenAI API | 1,050,000 | 128,000 | [Model documentation](https://developers.openai.com/api/docs/models/gpt-5.6-sol) |
| Claude Sonnet 4.6 | 1,000,000 | 128,000 | [Model documentation](https://platform.claude.com/docs/en/models/sonnet-4-6/overview) |
| GPT-5.6 Sol, Codex subscription | 872,000 | 128,000 (not enforced) | [Codex model catalog](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) |

The Codex catalog advertises a default context of 272,000 and a maximum configurable context
of 872,000, distinct from the public API's model limit. The example uses that catalog maximum.
Codex subscription does not accept an output-token limit on the wire. Its `max_output`
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

```toml
approve_all = false
capabilities = ["read", "write", "exec", "network", "agents", "mcp"]

[providers.local]
kind = "openai"
base_url = "http://127.0.0.1:8080/v1"
api = "chat_completions"

[models.local]
provider = "local"
model = "qwen3.8-27b"
max_context = 262144
max_output = 131072
supports_images = false
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

## Model connection recovery

If a Codex WebSocket connection is interrupted before its response is committed, Skyhook
reconnects and requests a fresh response using the same committed history. Recovery stays within
the existing root turn or child job: completed tools are not rerun, and tool calls from an
interrupted response are never executed. Partial streamed output is marked as interrupted and
is not added to the model's history. Inputs received during recovery wait until the next normal
request boundary.

Connection recovery allows three attempts in total, with cancellation-aware delays of roughly
500 ms and 1.5 s plus up to 100 ms jitter. The UI and session journal show scheduled reconnects;
a child is marked failed only after recovery is exhausted. Context-window compaction has a
separate, additive budget, with at most five attempts at a logical agent response when both
recovery paths are needed (excluding compaction's separate summary requests). These limits
reset after a successful model response.

Recovery applies to eligible Codex WebSocket close/EOF, I/O, write, ping, and read-timeout
failures—not authentication failures, invalid requests, malformed protocol responses, explicit
provider aborts, or user cancellation. Native HTTP startup retry and Codex handshake fallback
policies are unchanged. Reconnecting clears the failed connection and its continuation state,
so the next attempt sends full history rather than continuing the interrupted response.

This prevents duplicate **Skyhook tool execution**, not duplicate provider inference: the
provider may have processed the interrupted request, and additional usage may be incurred.
Usage reported before interruption is retained; unreported provider usage remains unknown.
Recovery is not a general replay guarantee for provider-hosted side-effecting tools.

## Reasoning history and local-server compatibility

Responses and Codex automatically request reasoning summaries (`reasoning.summary = "auto"`),
even when no model `reasoning` effort is configured. Configuring `reasoning` adds the effort without
changing that summary request. Skyhook displays the plaintext reasoning or summaries the provider
exposes; this does not guarantee access to raw OpenAI reasoning. What is returned depends on the
provider and model, and compatible servers may differ in support for these request fields.

All backends retain returned reasoning and replay state in the session journal. Responses and Codex
replay native reasoning items (including encrypted state), and Anthropic replays signed thinking or
redacted-thinking blocks. Native replay is automatic when provider, endpoint, protocol, and model
provenance match. Visible summaries are not substituted for opaque or signed state. Switching to an
incompatible provider/model filters replay from that request without deleting the original history.
Reasoning that the service never returns cannot be reconstructed.

Standard Chat Completions has no portable request-side reasoning field. Compatible Chat providers
replay their returned, scoped reasoning using **`reasoning_content` by default**: this is consumed by
llama.cpp and SGLang, and accepted as an alias by current vLLM. Ordinary Chat responses without
reasoning do not acquire an invented reasoning field. Configure a different spelling or disable
request replay on the **provider**, not individual model profiles:

```toml
[providers.local]
kind = "openai"
base_url = "http://127.0.0.1:8080/v1"
api = "chat_completions"
# Optional; this is the default:
chat_reasoning_replay = "reasoning_content"
# Alternatives: "reasoning", or "unsupported" to keep reasoning locally only.
```

This option belongs only to OpenAI-compatible Chat Completions. Anthropic and Codex reject it as an
unknown setting; configuring it with `api = "responses"` is also rejected. Their native reasoning
replay remains automatic and independent of this option. All models using a provider share its
Chat wire convention; use separate provider entries if a proxy routes to incompatible conventions.
The agent runtime and generic `ModelRequest` never interpret this setting.

Reasoning is replayed with its owning assistant turn, including tool calls and reasoning-only turns;
it is never merged into answer text or fabricated `<think>` tags. Old Chat transcripts that contain
only display text without scoped replay provenance cannot safely be upgraded to native replay.
Compaction retains complete selected assistant/tool exchanges; older exchanges may be summarized to
keep active context bounded, while original journal events remain available.

OpenAI-compatible and Anthropic providers also accept positive `startup_timeout_secs` and
`read_idle_timeout_secs` settings (both default to 600 seconds). Startup is a deadline for each HTTP
attempt, while read-idle resets after each response-body chunk. For example:

```toml
[providers.local]
kind = "openai"
base_url = "http://127.0.0.1:8080/v1"
api = "chat_completions"
startup_timeout_secs = 600
read_idle_timeout_secs = 600
```

The Chat codec accepts llama-swap loading chunks with an absent singleton choice index, repeated
same-tool updates within one chunk (such as vLLM Hermes), and late cache-usage attribution. It still
rejects malformed/nonzero choice indexes, multiple answer choices, and unsupported semantic
extensions. Server-side tool and reasoning parsers/templates must be configured appropriately;
Skyhook does not infer them from model names. The current `max_completion_tokens` and usage-stream
fields are shared by OpenAI, llama.cpp, vLLM, and SGLang; no automatic parameter renaming is applied.

Native HTTP transport owns startup retries, with **at most three HTTP attempts** for pre-response connection/send
failures, startup/header timeouts, and transient HTTP 408/429/500/502/503/504 responses. Each attempt receives a fresh startup
deadline; three startup timeouts can therefore take about 30 minutes at the defaults, plus bounded
backoff. Exhausted transient failures include the HTTP attempt count. Short `Retry-After` delays are honored;
long or unparseable delays are returned to the caller rather than retried early. Cancellation drops
the pending request or retry wait. The server may continue work if it does not honor disconnects.

After successful HTTP headers are accepted, malformed SSE streams, read-idle timeouts, and partial
output are not replayed. Deterministic protocol/configuration failures and refusals are not regenerated
by the agent loop. Context-overflow recovery remains a separate compaction path. Codex retains
single-attempt HTTP; eligible WebSocket interruptions use the bounded runtime recovery described
above. Aborted or truncated tool generation never makes incomplete arguments executable.

## Migrating older configurations

Migration from Flux is a breaking configuration change: replace `openai_compatible` with `openai`,
add explicit API-root URLs and OpenAI API choices, replace model aliases with full identifiers, and
log in separately for Codex.

See [authentication](authentication.md) for API-key and Codex login setup,
and [sessions and context](../guide/sessions-and-context.md#conversation-compaction) for
context budgeting and compaction.
