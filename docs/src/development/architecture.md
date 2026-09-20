# Library architecture

- `provider::Provider` is a shared factory: `open_context(correlation)` creates an owned
  `ProviderContext`, whose `invoke(&mut self, request)` returns an asynchronous response stream.
  Adapters live under `provider::backends` and wire types under `provider::protocol`.
  Custom providers implement both traits. A request's correlation must match its context's identity.
  Responses stream as `ResponseEvent`s: items and their blocks start, receive typed deltas, and
  end with authoritative final content; usage snapshots replace earlier values and
  `ResponseEnded` carries a stop reason. `ResponseAssembler` validates that lifecycle and supplies
  ordered snapshots to the runtime and to observers. Reasoning replay payloads keep their
  provider/model provenance, so incompatible private reasoning is omitted from requests rather
  than deleted from the transcript. Input images are encoded or rejected explicitly, never
  silently dropped; generated image outputs are not supported.
- Each agent loop owns an `AgentContext`: projected journal history, model profile and request
  template, token accounting, and its provider handle. Providers never internally replay a submitted
  request; the runtime retries an uncommitted response within its recovery policy.
  Handles survive turns, retries, questions, and compaction, and are released when the agent exits.
  Model-profile changes prepare a replacement before committing, preserve history, and reset token
  calibration. Equivalent profiles retain their handle. Resuming opens a fresh handle with the
  same agent cache identity; calibration carries across compaction.
- `tool::ToolRegistryBuilder` supports typed and JSON-based tools, while `tool::executor::ToolExecutor`
  turns every invocation into a supervised job. Typed registrations generate both input and output
  schemas; compact result shapes are included in model and script documentation.
- `session::SessionStore` persists each session in one SQLite database (`session.db`): a normalized
  append-only ledger committed one transaction per step, content-addressed blobs, and job outputs
  with line-addressable captures.
- `agent::Harness` owns model profiles and policy; each `agent::SessionHandle` owns an isolated agent tree
  and registry.

The `skyhook-agent` package holds the `skyhook` library crate and the CLI binary. The terminal
is a host of the same library used by embedders,
not a provider-specific agent implementation.

See [embedding](embedding.md) for host observation and replay contracts,
[remote transport and shims](remote-transport-and-shims.md) for backend boundaries, and
[jobs and agents](../scripting/jobs-and-agents.md) for delegation behavior.

Source: [library](https://github.com/kryesh/skyhook/tree/main/src)
and [CLI](https://github.com/kryesh/skyhook/tree/main/src/bin/skyhook).
