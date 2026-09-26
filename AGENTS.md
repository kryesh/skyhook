# Skyhook

- Library: `skyhook`.
- Binaries:
  - `skyhook` — CLI; feature `tui`.
  - `linux-ssh` — SSH shim; feature `shim-bin`.

Architecture: `docs/src/development/architecture.md`.

## Changes

- Stay within the requested scope and layer. Ask before changing unrequested tool
  schemas/descriptions, system prompts, config keys, CLI flags, or public functions.
- Delete superseded code. No redundant wrappers, comments, or compatibility shims;
  format changes are clean breaks.
- Extend shared mechanisms rather than adding parallel implementations. Ask before
  broadening scope to replace one.
- Document behavior, usage, and non-obvious rationale. Keep implementation details
  out of user docs; keep architecture docs accurate.

## Types

- Parse external data into enums/newtypes at boundaries. Keep string tags, dynamic
  JSON inspection, and error-message interpretation out of domain logic.
- Represent valid states directly with enums or typestate, not sentinels,
  interdependent booleans/options, or initialization-only `None`.
- Remove guards and tests made redundant by type guarantees; retain boundary validation.
- Use normalized SQL tables, foreign keys, and seeded dictionaries for repeated
  enumerations. No JSON payload columns for Skyhook-typed data, except documents the
  model received as tool output (tool results, job views), stored as the JSON it saw.

## Tests

- Test distinct behaviors, transitions, and boundary parsing—not compiler guarantees.
  Assert strings, formatting, and serialization shape only when contractual.
- Reuse fixtures; merge redundant tests, not unrelated behaviors.
- Wait for the relevant observable condition with a bounded timeout (`bounded` where
  available), not guessed delays or unrelated side effects.
  No `start_paused` with real child processes.
- Investigate load-sensitive failures. Stress new tests coordinating gated provider steps:

  ```sh
  cargo nextest run --all-features -E 'test(<name>)' --stress-count 200 --no-fail-fast
  ```
- Tests belong in a `tests` module at the bottom of the file containing the logic they
  test; tests of a module's functionality go in that module's top-level file. Avoid
  dedicated test files, except binary integration tests under `tests/`.

## Verify

```sh
just fmt
just lint
just test
```

For direct Cargo verification, use `--all-targets --all-features` where supported
to include the CLI/TUI.
