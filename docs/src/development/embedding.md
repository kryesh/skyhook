# Embedding

The `skyhook-agent-core` package exposes the `skyhook` Rust crate. Embedding hosts supply
configuration, a workspace, model selection, and interaction handlers rather than depending
on terminal UI state. See [library architecture](architecture.md) for the provider, registry,
and session ownership boundaries.

## Host observation API

`config.into_runtime()?.select_model(name)?.harness_builder(workspace)?` admits the configuration,
selects the host's explicit model choice, and returns a `HarnessBuilder` for that workspace.
`SessionHandle::observe()` returns an atomic snapshot/receiver pair with revisioned updates,
request-scoped live responses, current activity, and context estimates. On receiver lag, replace
both with a fresh observation. Durable records are identified by their original sequence.
`inspect_jobs` and `inspect_output` inspect metadata and saved output without claiming jobs or
consuming notifications; `cancel_job` explicitly requests cancellation. `SessionStore::read_records`
reads an archive without acquiring a writer lock or repairing a partial final line.

Library embedders manage their own process environment: loading a core `Config` does not
load `.env`. That startup behavior belongs to the CLI.

## Embedded shim catalog

The CLI injects its embedded shim catalog into the core harness. Shims are selected by protocol,
platform, and architecture; artifact names follow `platform-protocol-arch` (for example,
`linux-ssh-aarch64`). The naming scheme allows an optional `.exe` suffix for future platforms,
but no Windows transport is implemented. Library embedders receive an empty catalog by default
and can provide their own `EmbeddedShimCatalog` through `HarnessBuilder`; selecting a combination
without a supplied shim returns an unsupported-platform error.

## Reconstructing model calls

Session format 3 records the inputs needed to reconstruct each call at the shared `Provider`
boundary. It stores no backend-specific request bodies or authentication headers:

- `model_context` records the configured provider name and a shared `ModelRequest` template:
  actual model ID, assembled system prompt (including harness instructions and location),
  tool descriptions and schemas, optional response schema, reasoning setting, output limit, and correlation. Its `history`
  and `tail` arrays are empty; conversation history remains in `message_committed` and `compaction` events.
- `model_requested` is persisted before each provider invocation. It references the context event's
  sequence and records `history` as ordered source-event sequences (committed messages or compaction
  checkpoints), `tail` as exact inline messages (transient runtime state and compaction directives),
  and the request's `history_lifetime`. Its purpose distinguishes ordinary
  agent calls from summarization. Ordinary calls within a turn share one context record;
  summarization records a separate template containing its response schema.
- `compaction` is an ordinary log event containing the exact replacement message, retained original
  message references, covered frontier, previous compaction reference, and the
  summarization request reference and token estimates. It also contains schema version 1 and the
  reconciled owner todo list. History and todos become active together after persistence.
- Reconstruction resolves recorded references and inline values rather than using current
  configuration, prompt code, or live job state. It neither reruns summarization nor rerenders old
  compaction messages. Failed/interrupted calls retain their input records; usage records identify
  their originating request, including summarization calls.

`session::reconstruct_model_request(&records, sequence)` returns the provider name and reconstructed
`ModelRequest` for a `model_requested` sequence, using sequence-ordered records from `SessionStore`.
Attachments and tool images reference content-addressed session blobs by sha256;
`store.load_blobs(&mut request).await` loads their contents for provider encoding. This reconstructs
Skyhook's provider-neutral input, not an API-specific wire encoding.

A `ModelRequest` sends `history` (committed conversation, an unchanged prefix of later requests in the
same context until compaction replaces it) followed by `tail` (rebuilt for each request and never
cacheable); `request.messages()` iterates both in order. A model profile's `state_mode` decides where
runtime state goes: in the tail (`dynamic`), committed to history (`persist`), or nowhere (`none`).
`history_lifetime` is `continuing` (the default) when later requests in the context extend this
history, `ending` when compaction replaces it after this agent request (its own estimate already
reaches the compaction threshold), or `detached` for compaction summaries, which share no request
settings with any other request. Providers decide prompt-cache placement from `history`, `tail`, and
the lifetime: Anthropic places a cache breakpoint on the last history block unless it is detached
(ending history is still marked, since cache reads land only at breakpoints), OpenAI protocols rely on
automatic prefix caching with the tail sent last, and Codex records no WebSocket continuation for a
request with a tail or non-continuing history.
Session journals use format version 3; there is no compatibility or migration layer for earlier layouts.

Source: [session module](https://github.com/kryesh/skyhook/tree/main/crates/skyhook-core/src/session)
and [agent module](https://github.com/kryesh/skyhook/tree/main/crates/skyhook-core/src/agent).
