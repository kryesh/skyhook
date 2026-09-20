# Documentation and publishing

The book sources live in `docs/src`, navigation in `docs/src/SUMMARY.md`, and configuration in
`docs/book.toml`. Generated output goes to `docs/book`; it is ignored and must not be committed
to the source branch. Builds use the installed mdBook and its default theme.

## Writing and reviewing

Follow [AGENTS.md](https://github.com/kryesh/skyhook/blob/main/AGENTS.md): document behavior,
usage, and non-obvious rationale, and keep each chapter focused on its audience.

- **Getting started, guides, and configuration** explain what users can do, how to configure it,
  and which limits or security boundaries matter. Keep internal types, storage schemas,
  transport machinery, and rendering algorithms out of these chapters.
- **Scripting and reference** describe callable interfaces, result shapes, lifecycle behavior,
  and examples. Distinguish supported contracts from implementation details.
- **Development** covers library APIs and implementation responsibilities. Keep the
  [architecture](architecture.md) accurate when those responsibilities or boundaries change.

Describe the current behavior rather than accumulating historical implementation notes.
Give shared behavior one main explanation and link to it from other chapters. Check behavioral
claims and examples against the implementation, including defaults, limits, and failure cases.

Use relative chapter links such as `../guide/headless.md` inside the book, and
`https://github.com/kryesh/skyhook/blob/main/...` for repository files. The configuration
overview includes the root `config.example.toml` at build time instead of maintaining a second copy.

Keep secrets out of `docs/src`: mdBook copies every non-Markdown file there into the output,
ignored or not.

## Build and preview

```sh
just docs          # mdbook build docs
just docs-serve    # mdbook serve docs, normally http://localhost:3000
just pages-build   # build, then add .nojekyll for GitHub Pages
```

After editing, build the book and inspect the affected pages, examples, and links. A successful
build alone does not establish that examples are correct or linked pages exist.

## Publishing

```sh
just pages-publish [remote] [branch]   # defaults: origin, pages
```

Publishing is explicit, not part of a normal build. Choose a remote that points to the intended
GitHub repository. The recipe requires a clean checkout, builds the committed sources in a
temporary worktree, refuses to publish over a branch that contains sources, and pushes without
force. Your source checkout and local branches are untouched.

In the GitHub repository's **Settings → Pages**, choose **Deploy from a branch**, **`pages`**,
**`/ (root)`**. The documentation URL is <https://kryesh.github.io/skyhook/>.
