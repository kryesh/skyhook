# Library architecture

- `provider::Provider` is a shared factory: `open_context(correlation)` creates an owned
  `ProviderContext`, whose `invoke(&mut self, request)` returns an asynchronous response stream.
  Adapters live under `provider::backends` and wire types under `provider::protocol`.
  Custom providers implement both traits. A request's correlation must match its context's identity.
  The common `ResponseEvent` contract separates output items from independently streamed blocks:
  `ItemStarted`, `BlockStarted`, typed `BlockDelta`, `BlockEnded`, `ItemEnded`, `UsageUpdated`, and
  `ResponseEnded`. Item/block IDs identify content; explicit positions determine its order.
  Start events declare kinds, block ends contain authoritative final content, and item ends attach
  replay metadata once. Each readable reasoning part is a separate visible block with its own end;
  encrypted-only reasoning items need not create an empty visible section.
  `ResponseAssembler` validates lifecycles and supplies ordered snapshots to both the runtime and
  observation/UI layers. Cumulative usage snapshots replace earlier values, and response termination
  carries a stop reason rather than a truncation boolean. Legacy unindexed delta events are removed.
  Completed assistant messages persist nested items/blocks with their IDs and positions. Session
  journals now use format version 3; earlier journals are rejected with an explicit unsupported-version
  error rather than guessing the missing block boundaries. Start a new session after this upgrade.
  Reasoning
  replay payloads retain provider/endpoint/protocol/model provenance: incompatible private reasoning
  is omitted from outgoing requests, not deleted from the stored transcript.
  Input images are encoded or rejected explicitly, never silently dropped. Protocols without
  image-bearing tool results adapt images into adjacent user content associated with the tool call.
  Chat Completions also accepts the common `reasoning_content` / `reasoning` streaming fields used
  by local Qwen servers, preserving separate visible reasoning blocks and scoped replay envelopes.
  Chat request-side replay uses the provider's `chat_reasoning_replay` field selector (default
  `reasoning_content`);
  other APIs replay their compatible native reasoning state automatically. Null optional extension
  fields are tolerated; unsupported
  nonempty semantic fields fail explicitly. Generated image outputs are not supported.
  Chat streaming compatibility is provider-neutral: absent/null choices and deltas are normalized
  to empty containers, and metadata-only chunks are harmless before `[DONE]`. After `finish_reason`,
  usage and no-op deltas (empty/null text, empty tool-call lists, assistant role headers, and null
  extensions) remain accepted, with or without usage; identical finish reasons are idempotent.
  Extra envelope, choice, and usage metadata is ignored. Actual post-finish output, conflicting
  finish reasons, unsupported non-null delta fields, malformed tool calls or usage, and data after
  `[DONE]` still fail. Metadata never substitutes for a finish reason at EOF or `[DONE]`.
- Each agent loop owns an `AgentContext`: projected journal history, model profile and request
  template, token accounting, and its provider handle. Codex contexts have separate WebSocket
  connection and continuation state, with shared authentication tokens. Connection setup failures
  may fall back to HTTP. The provider never internally replays a submitted WebSocket request;
  the runtime can reset the connection and retry an uncommitted response within its recovery budget.
  Handles survive turns, retries, questions, and compaction, and are released when the agent exits.
  Model-profile changes prepare a replacement before committing, preserve history, and reset token
  calibration. Equivalent profiles retain their handle. Resuming opens a fresh handle with the
  same agent cache identity; compaction clears calibration from the previous history projection.
- `tool::ToolRegistryBuilder` supports typed and JSON-based tools, while `tool::executor::ToolExecutor`
  turns every invocation into a supervised job. Typed registrations generate both input and output
  schemas; compact result shapes are included in model and script documentation.
- `session::SessionStore` persists a versioned append-only JSONL log, content-addressed attachment and image blobs, job
  outputs, and line-addressable job output.
- `agent::Harness` owns model profiles and policy; each `agent::SessionHandle` owns an isolated agent tree
  and registry.

The CLI package is `skyhook-agent`; the source-only core package is `skyhook-agent-core`
with Rust crate name `skyhook`. The terminal is a host of the same core used by embedders,
not a provider-specific agent implementation.

See [embedding](embedding.md) for host observation and replay contracts,
[remote transport and shims](remote-transport-and-shims.md) for backend boundaries, and
[jobs and agents](../scripting/jobs-and-agents.md) for delegation behavior.

Source: [core library](https://github.com/kryesh/skyhook/tree/main/crates/skyhook-core/src)
and [CLI](https://github.com/kryesh/skyhook/tree/main/src/bin/skyhook).
