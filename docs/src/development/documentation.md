# Documentation and publishing

The book uses the installed `mdbook` with its default theme. Sources live in `docs/src`,
navigation in `docs/src/SUMMARY.md`, configuration in `docs/book.toml`; generated output is
`docs/book` (ignored, never committed to the source branch).

```sh
just docs          # mdbook build docs
just docs-serve    # mdbook serve docs, normally http://localhost:3000
just pages-build   # build, then add .nojekyll for GitHub Pages
```

Keep secrets out of `docs/src`: mdBook copies every non-Markdown file there into the output,
ignored or not. Use relative chapter links such as `../guide/headless.md` inside the book, and
`https://github.com/kryesh/skyhook/blob/main/...` for repository files. The configuration
overview includes the root `config.example.toml` at build time.

## Publishing

```sh
just pages-publish [remote] [branch]   # defaults: origin, pages
```

The recipe requires a clean checkout, builds the committed sources in a temporary worktree,
refuses to publish over a branch that contains sources, and pushes without force; your checkout
and branches are untouched. In the
GitHub repository's **Settings → Pages**, choose **Deploy from a branch**, **`pages`**, **`/ (root)`**.
The planned canonical URL is <https://kryesh.github.io/skyhook/>.
