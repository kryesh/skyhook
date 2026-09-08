# Skill tool fixture workspace

The hidden `.agents/skills/mixed-assets/` directory is a real discovery fixture.
It is deliberately outside this repository's active `.agents/skills/` directory,
so it is only discovered by runtimes explicitly using this fixture workspace.

Assets cover:

- `references/note.txt`: multilingual UTF-8 text.
- `references/config.json`, `settings.yaml`, and `data.csv`: structured text.
- `references/empty.txt`: an empty file.
- `scripts/example.py`: source text (tests do not execute it).
- `assets/diagram.svg`: SVG source, returned as text.
- `assets/pixel.png`: a valid 1×1 PNG, returned as an image attachment.
- `assets/payload.bin`: arbitrary non-UTF-8 binary bytes.
- `assets/nul.dat`: UTF-8-valid bytes containing NUL, treated as binary.

Core tests copy this layout into an isolated temporary workspace, discover it
with a fresh runtime, and exercise the native tool executor. Additional fixtures
for large output, symlinks, and permission failures are generated temporarily by
the tests rather than checked into the repository.

Run the skill tests with:

```sh
cargo test -p skyhook-agent-core tool::builtins::skills::tests
```

The disposable loopback SSH integration additionally verifies remote-agent native
calls, caller-workspace asset transfers, overwrite fidelity, and denied outside
paths. It requires `sshd`, `ssh-keygen`, and a locally built shim; it does not use
configured lab targets:

```sh
cargo build -p skyhook-agent --bin skyhook-shim --no-default-features --features shim-bin
SKYHOOK_TEST_SHIM="$PWD/target/debug/skyhook-shim" \
  cargo test -p skyhook-agent-core \
  host_skills_from_remote_caller_copy_into_workspace_override -- --ignored --nocapture
```
