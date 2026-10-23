# Providers and models

A provider entry names an endpoint and the models served through it, under `models`. Everywhere a
model is chosen or recorded—`--model`, the `/model` picker, the `agent` tool, `default_model`,
session history and `skyhook stats`—it is named `provider/model` from the two keys, so neither key
may contain `/`. The `model` field inside a profile is the identifier sent on the wire and stays
free-form. `default_model` selects the model a new session starts with when neither `--model` nor
the model last submitted in the workspace chooses one; without it, the first model of the first
provider is used.

Every provider names a **dialect**, the server's wire conventions. Each model selects a **codec**,
either directly or from its provider's defaults: `chat_completions`, `responses`, or `messages`.
The codec is the API family; `base_url` names the shared API root and Skyhook appends
`/chat/completions`, `/responses`, or `/messages` for the selected model.
Different models on one provider may use different supported codecs:

| Dialect | Codecs | Use for |
| --- | --- | --- |
| `compatible` | any | Standards-following servers: OpenAI- or Anthropic-compatible endpoints need only `base_url`. A key is sent as each API specifies: a bearer token, or `x-api-key` on `messages`. Response schemas are sent as written, in strict mode on `responses`. Extra field: `state_role` (default `user`). |
| `openai` | `chat_completions`, `responses` | `base_url` defaults to `https://api.openai.com/v1`. Response schemas are sent in strict mode, prompt caching is keyed per conversation, and Chat keeps reasoning local because the API accepts none back. Reasoning summaries need a verified organisation; an unverified one is refused, so set `reasoning_summary: null` for it. Extra fields: `organization`, `project`. |
| `anthropic` | `messages` | `base_url` defaults to `https://api.anthropic.com/v1`. Extra fields: `workspace_id`, `cache_ttl: "5m"` or `"1h"`, `state_role` (default `system`). |
| `codex` | `responses` | The ChatGPT subscription service; no `api_key`—run `skyhook auth login <provider>`. `base_url` defaults to the ChatGPT backend root `https://chatgpt.com/backend-api` and the extra field `auth_url` to `https://auth.openai.com`; set them only for a mirror. Response schemas are sent in strict mode. |
| `openrouter` | any | `base_url` defaults to `https://openrouter.ai/api/v1`. Reasoning effort and returned reasoning follow OpenRouter's conventions, history is marked for prompt caching (on Responses, through OpenRouter's automatic caching), and each conversation keeps its routing affinity. Response schemas are sent in strict mode on `chat_completions` and `responses`. Extra fields: `routing`, `cache_ttl: "5m"` or `"1h"` on every codec, and `state_role` (default `user`). `routing` is sent on every codec as OpenRouter's `provider` preferences and `models` fallbacks: `order` (provider slugs to try first; a preference unless `allow_fallbacks: false`, which restricts routing to them), `only` (provider slugs allowed), `ignore` (provider slugs skipped), `allow_fallbacks`, `require_parameters`, `data_collection: "allow"` or `"deny"`, `quantizations`, `zdr`, and `fallback_models`. A response schema always sets `require_parameters`. |
| `litellm` | any | A LiteLLM proxy; `api_key` is the virtual key, sent as a bearer token for every codec, including `messages`. Each model needs `upstream: "openai"`, `"anthropic"` or `"bedrock"` after inheritance. This asserts the family behind the alias; it does not configure proxy routing. Behind `anthropic`, Chat replays signed thinking and takes prompt caching. Behind `openai`, Chat and Responses take OpenAI's conventions, with neither. Behind `bedrock`, which serves both families, they take OpenAI's conventions too, but Chat still replays the thinking blocks the proxy returns, and both Chat and `messages` replay unsigned thinking until signed thinking appears. Response schemas are sent in strict mode on `chat_completions` and `responses`, except behind `anthropic`, which does not honour it. `messages` behind an Anthropic upstream reaches the service as sent and gets the `anthropic` dialect's conventions: turn-scoped system state, signed thinking kept, and the betas both need, which the proxy forwards. Behind Bedrock it does not, since LiteLLM drops the turn-scoped beta on its Bedrock path; behind OpenAI it goes through the proxy's translation, which drops reasoning. Chat, and `messages` behind Bedrock, drop superseded state, which breaks models that bind thinking to the exact earlier conversation (Claude Fable 5.1, Opus 5.5, Sonnet 5.5, Haiku 5.5): use `messages` behind Anthropic for them, or `state_mode: persist`. Each conversation's identity rides `x-litellm-session-id`, which LiteLLM's session affinity and spend logs key on but which it forwards to no service; Chat and Responses behind `openai` or `bedrock` also send it as `prompt_cache_key`, which LiteLLM passes on for the service's own cache affinity. LiteLLM refuses `prompt_cache_key` for every model it serves through Bedrock's Converse API: Claude, Nova, Llama, Mistral, and OpenAI-family models such as GPT-OSS unless they are routed to Bedrock's OpenAI endpoint. Serve Claude through `messages`, whatever its upstream. For the others, send the identity in the header alone with `cache_key: {header: "x-litellm-session-id"}`. Optional `tags` select LiteLLM's tag-based routing and label its spend logs. Extra field: `state_role` (default `system` for `messages` behind Anthropic, `user` otherwise). |

Every dialect but `codex` accepts `api_key` (see [authentication](authentication.md)); every
dialect accepts fixed request `headers` for proxies or attribution, the
[connection timeouts](#connection-timeouts), and `models`. On `messages`, a configured
`anthropic-beta` adds to the betas Skyhook announces rather than replacing them. The credential
header and header placements (`cache_key: {header: …}`, or a dialect's own) replace an entry
header of the same name; so does `Accept`, which is always `text/event-stream`. A shadowed
header's command does not run for that request. There are no model aliases or automatic vendor
detection: `model` is the identifier sent on the wire.

## Provider defaults and model settings

Request settings are flat: put shared defaults beside `models`, and model-specific values
directly beside `model`.

| Where | Fields |
| --- | --- |
| Provider only | `dialect`, `base_url`, `api_key`, `headers`, `startup_timeout_secs`, `read_idle_timeout_secs`; Codex's `auth_url`. |
| Provider defaults or direct model settings | `codec`; `service_tier`; all placement selections listed below; the Responses settings `reasoning_summary`, `access_programs`; `explicit_caching`; `image_limits`; the dialect's request fields: OpenAI's `organization`, `project`; Anthropic's `workspace_id`, `cache_ttl`, `state_role`; OpenRouter's `routing`, `cache_ttl`, `state_role`; LiteLLM's `upstream`, `tags`, `state_role`; Compatible's `state_role`. |
| Model only | `model`, `reasoning`, `max_context`, `max_output`, `supports_images`, `state_mode`, `hint`. |

Missing model settings inherit from the provider. Explicit values win even when they are `false`,
`[]`, or a setting's usual default. Lists replace inherited lists rather than append. Under
OpenRouter's `routing`, only declared members change: a model's `order` replaces the inherited
order while an undeclared `allow_fallbacks` still inherits. The same leaf-by-leaf inheritance
applies to `access_programs` and `image_limits`: `{}` inherits its leaves, rather than clearing
the object.

Every request setting in the table above reads the same way, apart from lists. Left out, it
inherits, and with nothing to inherit takes its default. An explicit `null` sends nothing: `organization: null` removes the
organization header, `routing: null` removes routing preferences, `reasoning_summary: null`
requests no summaries, and a placement selection of `null` places nothing. A setting that always
sends something, such as `state_role` or `reasoning_effort`, refuses `null`; leave it out to take
its default. Lists, such as LiteLLM's `tags`, refuse `null` too: `[]` is the empty list. A
placement selection's list of locations is never empty; `null` is how it places nothing.
Provider-only settings differ: `startup_timeout_secs: null`, for one, takes the default.

Every model must have `codec`, and LiteLLM models must also have `upstream`, **after inheritance**.
A provider may omit either if every model supplies it. Defaults and compatibility checks apply to
each effective model, not to the provider defaults in isolation. Unknown fields, provider-only
fields under a model, fields for another dialect, and unsupported codec/setting combinations are
rejected. API keys, credential-command caches, and connection resources remain shared by all
models in one provider instance, regardless of their request settings.

For example, one proxy can serve different families and APIs without duplicating credentials:

```yaml
providers:
  proxy:
    dialect: "litellm"
    base_url: "https://litellm.example.com/v1"
    api_key:
      env: "LITELLM_API_KEY"
    tags: ["skyhook"]
    models:
      claude:
        model: "claude-alias"
        codec: "messages"
        upstream: "anthropic"
        max_context: 200000
        max_output: 64000
      gpt:
        model: "gpt-alias"
        codec: "responses"
        upstream: "openai"
        tags: []                  # Do not inherit the provider's tags.
        max_context: 1050000
        max_output: 128000
```

Configure the aliases and routing in LiteLLM itself; `upstream` tells Skyhook which request and
reasoning conventions to use for each alias.

### Service tier

`service_tier` selects the backend's tier ID, such as `priority`, `flex`, or `ultrafast`, on any
dialect and codec, as a provider default or beside `model`. It is unset by default and sent
exactly as written: Skyhook does not translate client aliases such as `fast`. Check that the
endpoint and model accept the tier before selecting it. Faster tiers can cost more or use more of
a subscription; see [ChatGPT speed](https://learn.chatgpt.com/docs/agent-configuration/speed).

### Responses settings

Any dialect serving `codec: "responses"` accepts two optional settings, as provider defaults
or beside `model`. A backend serving Responses need not accept them; check that the endpoint,
model, and account support a setting before selecting it. Other codecs refuse a selection, so a
model that switches away from Responses must clear inherited ones with `null`.

- `reasoning_summary` is the detail of the reasoning summaries requested: `"auto"` (every
  dialect's default), `"concise"` or `"detailed"`, or `null` to request none.
- `access_programs.cyber` requests a cybersecurity access program: `standard`, `daybreak_blue`,
  or `daybreak_red`; unset by default. The model and account must be eligible for the program; it does not grant
  entitlement, and other safeguards still apply. Some models take a different wire value from
  the approval it needs: `gpt-6-astra` and `gpt-6.1-sol` take `daybreak_blue` but require Daybreak
  Red entitlement. See the [Daybreak guide](https://developers.openai.com/api/docs/guides/daybreak).

### Explicit caching

`explicit_caching: true`, as a provider default or beside `model`, switches a
`chat_completions` or `responses` request to explicit prompt caching
(`prompt_cache_options: {mode: "explicit"}`): the backend caches only at the breakpoints
Skyhook marks, never on the changing runtime state, so later requests can reuse the prefix.
Both mark the end of the stable history and the point where each earlier request's history
ended, and Chat also the stable system prompt, up to four breakpoints a request: the newest
history boundaries are kept. A dialect that already marks breakpoints with `cache_control` on
Chat (`openrouter`, and `litellm` behind `anthropic`) places the same marks with that field.
On `openrouter` Responses, `cache_ttl` applies to automatic caching only; explicit marks are
cached for the backend's default lifetime.
Neither marks the history of a request with no reusable prefix, such as compaction. Left unset, the backend's
automatic caching is unchanged. `messages` refuses the setting. Check that the endpoint and
model support it; see the
[prompt-caching guide](https://developers.openai.com/api/docs/guides/prompt-caching) for
supported models and pricing.

```yaml
astra:
  model: "gpt-6-astra"
  max_context: 1050000
  max_output: 128000
  service_tier: "priority"
  access_programs:
    cyber: "daybreak_blue"
  explicit_caching: true
```

### Image limits

Images of up to 8 MiB are stored as attached, except that an animation, and any GIF, is stored as
a PNG of its first frame (an APNG's default image, which may lie outside its animation),
re-encoded as a smaller JPEG if that PNG would exceed 8 MiB. An image with very large dimensions
can be refused when attached, whatever the model's limits.
Before a request, an image whose longest edge or encoded size exceeds the model's limits is
fitted: it is downscaled to the edge limit, keeping its aspect ratio, and re-encoded as PNG
(unless it is a JPEG) if that is within the size limit; otherwise it is re-encoded as JPEG and
made smaller until it fits. An image within the limits is sent unchanged, the stored original is
kept, and the model is not told that an image was fitted. An image that cannot be fitted, such as
one whose stored data no longer decodes, is sent as a short note saying so, and the request
proceeds. Each dialect sets defaults:

| Dialect | `max_edge` | `max_bytes` |
| --- | --- | --- |
| `anthropic` | 8000 px | 7.5 MiB (10 MiB as base64) |
| `openai`, `codex` | 6000 px | 8 MiB |
| `compatible`, `litellm`, `openrouter` | 4096 px | 3.5 MiB |

`image_limits` replaces either limit, as a provider default or beside `model`; a limit left out
keeps the default, and `image_limits: null` restores both. `max_bytes` is at most 8 MiB, since no
image is stored larger, and `max_edge` at most 65535 px, the largest JPEG; a larger value reads
as that maximum. The limits are per image: a service
rule that depends on how many images a request holds, or on their total pixel area, is not
applied. Changing a model's limits from its dialect's defaults changes the images earlier
reasoning saw, so signed reasoning from before the change is not replayed afterward.

```yaml
local:
  model: "vision-model"
  image_limits:
    max_edge: 2048     # longest edge, in pixels
    max_bytes: 2097152 # encoded size, in bytes
```

### Placement selections

A server that deviates from its dialect's presets can select placements at either level:

```yaml
providers:
  local:
    codec: "chat_completions"
    dialect: "compatible"
    base_url: "http://127.0.0.1:8000/v1"
    output_limit: "max_tokens"                   # or null for none
    reasoning_replay: "reasoning"                # the assistant-message key; or null
    cache_key:                                   # one location, a list of them, or null
      - body: "cache_salt"
      - header: "x-session-id"
    models:
      thinker:
        model: "served-name"
        max_context: 131072
        max_output: 32768
        reasoning_effort: "reasoning_effort"
        output_limit: null                      # Send no output limit.
```

The selectable dimensions are `output_limit`, `reasoning_effort`, `reasoning_replay` and
`tool_stream` (Chat Completions only), `cache_key`, and `user_id`. Every one but `cache_key` is a
field path. Each selection names one location or a nonempty list of distinct locations, and the
value is written at every one. A selection left
out inherits, and with nothing to inherit keeps the preset for the model's effective codec and
dialect settings; an explicit `null` places nothing. Changing `codec` does not discard inherited
selections: set `null` for any the new codec has no place for, or replace them.

A field path is dot-separated (`metadata.user_id`); `cache_key` and `user_id` carry the
conversation identity.
Paths must not name a field the codec writes itself (such as `model` or `messages`) or one the
dialect adds (such as OpenRouter's routing `provider` and `models`), and no two may
overlap, including one inside another (`reasoning` and `reasoning.effort`), within one selection or
across two. A `cache_key` header
must not be `accept`, `content-type`, or a header the codec sends itself (such as
`anthropic-version` on `messages`). A `reasoning_replay`
field moves the provider's replay to other keys in the same form. Messages requires an output
limit, so `output_limit: null` is refused there, as is `reasoning_effort: null` everywhere.

## Context and output budgets

Every model profile requires `max_context` and `max_output`. Both must be positive, and
`max_output` must be smaller than `max_context`. Prompt/history, reasoning, and generated
text share the context window. Automatic [compaction](../guide/sessions-and-context.md#conversation-compaction)
is triggered by a response whose reported input, cached input, and output tokens reach
90% of `max_context - max_output`; it does not estimate the next request. Compaction precedes
tool execution, or, after a final answer, the agent's next request. The summarizer sees the
pending calls, and their results reach the first post-compaction request intact; a pending `todo`
replacement is superseded by its reconciled list.
A `max_output` of at least 50% of `max_context` produces a startup warning for that model, whether
or not it is selected: reserving that much output space leaves most of the window unused for
input. The configuration remains valid.
Normal requests retain their configured output limit; compaction requests reduce it when
necessary to fit the remaining context space.
Use the model's published limits or your server's actual configured limits, whichever is lower.
You can deliberately choose smaller budgets for cost, latency, or available memory. However,
using Skyhook with less than approximately **128k tokens of `max_context` is not recommended**:
instructions, tool definitions, conversation, and compaction all need room. This is guidance,
not an enforced minimum.

`state_mode` controls how per-request runtime state (date, active jobs, and todos) reaches the model.
`dynamic` (the default) sends a fresh state with each request; the model sees only the current one.
`persist` keeps each state in the conversation, at the cost of keeping every one in context until
compaction. Use it for models that bind signed reasoning to the exact earlier conversation (such
as Claude Fable 5.1) when the provider cannot hide earlier state, for example Claude reached
through a Chat Completions or Responses proxy. `none` sends no runtime state, except to a
compaction summary, which needs the current todo list to reconcile it. See
[runtime state](../reference/runtime-state.md) for what the model receives.

Runtime state is the harness speaking, so it never appears as tool output. A dialect's
`state_role` selects how it is sent:

- `system` uses the protocol's harness role: a system message on `messages`, a system or
  developer message on `chat_completions`, and a developer message on `responses`. Through
  `anthropic`, or `litellm` with an Anthropic upstream on `messages`, a `dynamic` state is a
  turn-scoped system message: the model reads it for one request, and afterwards it costs no input
  tokens but stays in the conversation, so models that bind thinking to the exact earlier
  conversation (Claude Fable 5.1, Opus 5.5, Sonnet 5.5, Haiku 5.5) keep it with `dynamic` state.
  It needs a model that supports mid-conversation system messages (Claude Opus 4.8 and later,
  Fable, Sonnet 5.5, Haiku 5.5; not Sonnet 5). Other dialects send plain system messages and drop
  superseded state, as `user` does. On `messages`, a state with no valid place, such as right
  after an assistant turn, is not sent.
- `user` sends state as user content, joining the user turn before it or as a turn of its own,
  for servers without a harness role mid-conversation, such as many open-model chat templates.
  Superseded state is then dropped, which changes the end of the conversation between requests.
  During a tool loop each state is a new user turn, so a chat template that keeps reasoning
  only for the latest user query drops the loop's earlier reasoning after it, whatever the
  `state_mode`. For such models, enable the template's option to keep earlier reasoning
  (Qwen's `preserve_thinking`, for example).

`anthropic` defaults to `system`, as does `litellm` for `messages` behind an Anthropic upstream;
`openai` and `codex` always use it, and `compatible`, `openrouter` and every other `litellm`
combination default to `user`. Changing a provider's `state_role` changes the conversation
earlier reasoning was produced in, so that reasoning is not replayed afterward. Through
`anthropic`, or `litellm` with an Anthropic upstream on `messages`, set `reasoning` to a level
other than `off` on models that bind thinking: only then does Skyhook ask the service to drop a
thinking block whose earlier conversation changed, rather than reject the request.

The [complete example](overview.md#complete-example) uses published hosted-model limits:

| Model | Context | Output | Source |
| --- | ---: | ---: | --- |
| GPT-5.6 / GPT-5.6 Sol, OpenAI API | 1,050,000 | 128,000 | [Model documentation](https://developers.openai.com/api/docs/models/gpt-5.6-sol) |
| Claude Opus 5.5 | 1,000,000 | 128,000 | [Model overview](https://platform.claude.com/docs/en/about-claude/models/overview) |
| GPT-5.6 Sol, Codex subscription | 872,000 | 128,000 (not enforced) | [Codex model catalog](https://github.com/openai/codex/blob/main/codex-rs/models-manager/models.json) |

The Codex catalog advertises a default context of 272,000 and a maximum configurable context
of 872,000, distinct from the public API's model limit. The example uses that catalog maximum.
Codex subscription does not accept an output-token limit. Its `max_output`
profile field remains required and records GPT-5.6 Sol's published output limit. It reserves
space in the compaction threshold, but does not enforce an endpoint output limit. Actual endpoint
limits remain authoritative.

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

On `chat_completions` and `responses`, a model's `reasoning` is an effort level: `none`, `minimal`,
`low`, `medium`, `high`, `xhigh` or `max`. On `messages` it is a thinking mode (`off` or `adaptive`)
or an effort level under adaptive thinking: `low`, `medium`, `high`, `xhigh` or `max`. A value the
codec does not accept is refused when the configuration loads, with the accepted values listed.

Responses and Codex automatically request reasoning summaries, even when no model `reasoning`
effort is configured. Anthropic requests summarized thinking when `reasoning` enables thinking;
with `reasoning` unset, the model's default applies and may omit thinking text.
Skyhook displays only the reasoning text or summaries the service returns, not otherwise-private
reasoning. Support varies by provider, model, and compatible server.

Skyhook saves returned reasoning and automatically reuses it with the same API and model.
Signed or encrypted reasoning is reused only with the same provider, endpoint, and configured
routing or tenant identity; switching to another configuration omits it from requests without
deleting the saved history. Readable reasoning is reused across those changes.
A proxy changing the upstream behind an unchanged alias cannot be detected automatically. Signed thinking cannot be reused across compaction
or mode switches. Visible summaries cannot replace private reasoning state the service requires.
Reasoning the service did not sign or encrypt is still replayed, until signed or encrypted
reasoning from the model appears in the context; from then on only that is sent. Services that
accept only reasoning they signed or encrypted never get the rest: `anthropic`, `openai` and
`codex` on `responses`, and `litellm` behind Anthropic and on `responses`.

Standard Chat Completions has no portable request-side reasoning field. Compatible Chat providers
replay their returned reasoning under **`reasoning_content` by default**. Ordinary Chat responses
without reasoning do not acquire an invented reasoning field.
A server that spells the key differently selects it with `reasoning_replay: "reasoning"`; `reasoning_replay: null` keeps reasoning local. The `openai` dialect never
replays on Chat, since the official API rejects unknown message keys; `litellm` behind an
`anthropic` or `bedrock` upstream replays the thinking blocks the proxy returns, and `openrouter` replays its
`reasoning_details`; signed ones are bound to the exact conversation like Messages thinking. A
response with readable `reasoning_details` keeps those as its reasoning, in place of any plain-text
reasoning it also streamed; a response without them keeps its plain text, which `openrouter`
replays under its `reasoning` field. Once signed reasoning appears in the context, only signed
thinking blocks are sent, a turn's `reasoning_details` are sent whole, in order, only if one of them
is signed, and plain-text reasoning stays local. Responses and Messages replay native reasoning
automatically; a Responses reasoning item that restates its summary as content, or the reverse,
is kept once, as its final item states.

Reasoning is replayed with its owning assistant turn, including turns that only call tools;
it is never merged into answer text or fabricated `<think>` tags. Server-side tool and reasoning
parsers/templates must be configured appropriately; Skyhook does not infer them from model names.
Chat requests send `max_output` as `max_completion_tokens`; a server that only honours
`max_tokens` sets `output_limit: "max_tokens"`.

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
