# Instructions and skills

## Instructions

Skyhook loads at most **one user instruction file** and **one file in the exact resolved
workspace directory**. It does not walk workspace ancestors for instructions. `--workspace PATH`
selects that workspace; `--config` selects only YAML configuration and does **not** suppress
instruction discovery.

Within each directory, filenames are checked in this order:

1. `AGENTS.md`
2. `agents.md`
3. `Agents.md`
4. `AGENTS.MD`

The first existing filename is selected. If reading it fails, Skyhook does not try a lower-priority
spelling in the same directory. A readable empty file counts as selected, so it does not trigger
fallback either. An instruction file must be a regular UTF-8 file of at most 1 MiB; a larger file or one that is not a regular file is skipped with a startup warning, and other read failures stop startup.

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

Host-owned skills are exposed through the `skill` tool, which is offered only when at least one
skill was discovered. Unlike instructions, skill discovery
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

Skill YAML frontmatter may contain arbitrary metadata, but its values must be JSON-compatible:
mappings with string keys, sequences, strings, booleans, finite numbers, and null. Duplicate or
non-string keys, custom tags, non-finite numbers, and YAML `<<` merge keys are rejected. Anchors
and aliases are supported with bounded expansion. A `description` must be a string; a skill with
any other non-null description is skipped with a warning.

## Loading and copying assets

The examples below assume the `release` skill layout shown above.

All `skill` arguments are optional and nullable; native calls supplying `null` behave like calls
omitting those fields. Without `name`, the call lists available skills; `path` requires `name`.

```js
await tool.skill(); // {kind: "list", skills: [{name, description}]}
await tool.skill({ name: "release" }); // Complete SKILL.md, assets tree, and location
await tool.skill({ name: "release", path: "references" }); // Directory subtree
await tool.skill({ name: "release", path: "references/template.md" }); // Original text
await tool.skill({ name: "release", path: "assets/logo.png" }); // Attached image
```

Results have a `kind` discriminator: `list`, `skill`, `directory`, `text`, `image`, or `binary`.
Base skill results contain `name`, `description`, `location`, complete `content`, and an `assets`
text tree that includes nested files but excludes the already-loaded root `SKILL.md`. Directory
results use the same tree format, rooted at the selected path. Trees mark symlinks without
descending through them. Long trees and asset text are saved in full and can be paged using `jobs`.

Text assets (including JSON, YAML, source code, and SVG) are returned unchanged, never executed.
Supported raster images are attached with metadata. Other binary files return metadata rather
than raw binary or base64 content. Paths inside a skill must be relative and cannot escape its
root. Use `path: "."` to browse the root.

Skills live on the session host. `location` is `{path, target: "root"}`, the skill directory's
absolute path there, so ordinary tools can reach its files. `target` is included only for agents
with the `targets` capability; an agent on a remote target without it cannot reach the session
host, so its results have no `location`. Copy an asset with
[`write`'s `source`](../reference/tools.md#creating-files-with-write), which also delivers it to
an agent working on a remote target:

```js
const { location } = (await tool.skill({ name: "release" })).unwrap();
await tool.write({
  path: "tmp/release.py",
  source: { path: `${location.path}/scripts/release.py`, target: "root" },
});
```

An agent on the session host can omit `target`.

A [mixed-file test workspace](https://github.com/kryesh/skyhook/tree/main/tests/fixtures/skill-workspace) includes its hidden
`.agents/skills/mixed-assets/` directory. Its tests exercise native calls, explicit nulls,
asset discovery, UTF-8 and binary files, image attachments, and path containment.
