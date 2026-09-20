# Instructions and skills

## Instructions

Skyhook loads at most **one user instruction file** and **one file in the exact resolved
workspace directory**. It does not walk workspace ancestors for instructions. `--workspace PATH`
selects that workspace; `--config` selects only TOML configuration and does **not** suppress
instruction discovery.

Within each directory, filenames are checked in this order:

1. `AGENTS.md`
2. `agents.md`
3. `Agents.md`
4. `AGENTS.MD`

The first existing filename is selected. If reading it fails, Skyhook does not try a lower-priority
spelling in the same directory. A readable empty file counts as selected, so it does not trigger
fallback either.

For user instructions, Skyhook first checks `$XDG_CONFIG_HOME/skyhook`, then falls back to
`$HOME/.config/skyhook` if the first location is absent or fails. It never loads both user files.
If no fallback succeeds, all non-missing user-file errors are reported. Missing optional files
are fine; an unreadable selected workspace file is an error.

For example, on a case-sensitive filesystem:

```text
$XDG_CONFIG_HOME/skyhook/agents.md  # Selected user file if AGENTS.md is absent.
$HOME/.config/skyhook/AGENTS.md     # Not loaded when the XDG selection is readable.
/project/AGENTS.md                 # Not loaded for workspace /project/app.
/project/app/AGENTS.md             # Selected workspace file, even if empty.
/project/app/agents.md             # Ignored when AGENTS.md exists, even if unreadable.
```

User instructions are supplied before workspace instructions, with their source identified.
If both locations select the same file, it is loaded only once. Root and child agents share
these instructions.

## Host-owned skills

Host-owned skills are exposed through `skills` and `skill`. Unlike instructions, skill discovery
reads `~/.agents/skills` and `.agents/skills` directories along the workspace ancestry, with the
nearest workspace definition winning when names collide. Discovery happens at startup;
restart Skyhook to discover newly added skills. A skill directory contains `SKILL.md` and optional supporting
files, conventionally grouped in `scripts/`, `references/`, and `assets/`:

```text
.agents/skills/release/
├── SKILL.md
├── assets/
│   └── logo.png
├── references/
│   └── template.md
└── scripts/
    └── release.py
```

## Loading and copying assets

The examples below assume the `release` skill layout shown above.

Only `name` is required by the `skill` input schema. `path` and `to` are optional and nullable;
native calls supplying `null` behave like calls omitting those fields.

```js
await tool.skill({ name: "release" }); // Complete SKILL.md plus recursive assets tree
await tool.skill({ name: "release", path: "references" }); // Directory subtree
await tool.skill({ name: "release", path: "references/template.md" }); // Original text
await tool.skill({ name: "release", path: "assets/logo.png" }); // Attached image
await tool.skill({ name: "release", path: "assets/logo.png", to: "tmp/logo.png" }); // Copy
```

Results have a `kind` discriminator: `skill`, `directory`, `text`, `image`, `binary`, or `copied`.
Base skill results contain `name`, `description`, complete `content`, and an `assets` text tree
that includes nested files but excludes the already-loaded root `SKILL.md`. Directory results
use the same tree format, rooted at the selected path. Trees mark symlinks without descending
through them. Long trees and asset text are saved in full and can be paged using `job_output`.

Text assets (including JSON, YAML, source code, and SVG) are returned unchanged, never executed.
Supported raster images are attached with metadata. Other binary files return metadata and
a suggestion to supply `to`, rather than raw binary or base64 content. Copies return destination,
byte count, and SHA-256, and require write authorization. `to` is resolved in the **calling agent's
target and workspace**: a remote agent receives host-owned asset bytes on its remote target,
not in a similarly named directory on the host. Skill discovery, instructions, and asset reads
remain host-owned. Remote copies require route and destination write authorization, including
path approval when the destination is outside the authorized workspace. `to` requires a file
`path`; paths inside a skill must be relative and cannot escape its root. Use `path: "."` to
browse the root.

A [mixed-file test workspace](https://github.com/kryesh/skyhook/tree/main/tests/fixtures/skill-workspace) includes its hidden
`.agents/skills/mixed-assets/` directory. Its tests exercise native calls, explicit nulls,
asset discovery, UTF-8 and binary files, image attachments, copying, and path containment.
