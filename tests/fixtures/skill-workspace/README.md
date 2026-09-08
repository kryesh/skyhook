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
- `assets/vision.png`, `vision-other.png`: distinct 320×240 color/shape charts for live visual acceptance (large enough for reliable image preprocessing).
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

Live native-provider acceptance uses the actual harness in this workspace and a dedicated
`target/live-skill-fixture/<profile>/` session root. It verifies image interpretation through both
a direct `skill` call and a background `script` → `job_output` call using different charts, checks
request-local image hydration, and fails on any model-request retries. These tests make real model
requests and are never enabled by default:

```sh
SKYHOOK_LIVE_PROFILE=terra cargo test -p skyhook-agent-core --test live_skill_fixture -- --ignored --nocapture
SKYHOOK_LIVE_PROFILE=qwen36 cargo test -p skyhook-agent-core --test live_skill_fixture -- --ignored --nocapture
```

Profiles come from the user config. Codex uses the Skyhook-owned login; Terra runs with low
reasoning effort to keep cost down. The harness disables write/exec/child-agent capabilities.

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
