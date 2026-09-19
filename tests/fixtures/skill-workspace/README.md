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
cargo nextest run tool::builtins::skills::tests
```

Remote skill-transfer tests verify caller-workspace routing, host-owned reads, and
write authorization without requiring SSH:

```sh
cargo nextest run tool::builtins::skill_transfer::tests
```
