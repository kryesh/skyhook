# Documentation and publishing

## Authoring and local preview

The book uses the installed `mdbook` executable and its default HTML theme. There is no
version pin, downloader, custom preprocessor, or theme bootstrap. An installed `just` provides
the task interface; direct mdBook commands work too.

```sh
just docs                 # mdbook build docs
just docs-serve           # mdbook serve docs
```

Open the address printed by `mdbook serve` (normally <http://localhost:3000>).
The book sources live in `docs/src`, navigation in `docs/src/SUMMARY.md`, and configuration in
`docs/book.toml`. Generated output is `docs/book`, ignored by `docs/.gitignore`.
Do not commit generated output to the source branch. Keep secrets out of `docs/src`: mdBook
copies non-Markdown assets, including ignored files, into local preview output. Git ignore rules
are not an asset filter for `just docs` or `just docs-serve`.

Use relative chapter links such as `../guide/headless.md` inside the book. Link to repository
source files using `https://github.com/kryesh/skyhook/blob/main/...` or `tree/main/...`, not paths
outside the book: those relative paths would break on the published site. The configuration
overview includes the root `config.example.toml` at build time so it stays the single source
of truth. Repository and edit links in the book also point to GitHub.

Check links after editing:

```sh
just docs-check
# Equivalently:
mdbook build docs
python3 scripts/check-docs.py
```

The checker requires Python 3.11+ and uses only the standard library. It validates rendered local
links and anchors, plus root README relative links; it does not fetch external URLs.
Code examples are documentation, not commands that the documentation build executes.

## Prepare the Pages artifact

The planned canonical URL is <https://kryesh.github.io/skyhook/>. Building documentation locally
does not publish that site or establish that it is already available.

```sh
just pages-build
```

This builds `docs/book` and adds `.nojekyll`, so GitHub Pages serves the generated static HTML
without Jekyll processing. To do the same directly:

```sh
mdbook build docs
touch docs/book/.nojekyll
```

## Publish manually

Publishing requires Git, an installed mdBook/just toolchain, push access to the destination
repository, and a **clean, committed source checkout**. Review and commit source changes first.
The recipe builds the book from an isolated checkout of the committed source revision, then
publishes through a separate temporary Git worktree. Ignored local files (including environment
secrets under `docs/src`) and stale local `docs/book` output are not inputs to publication.
It never force-pushes, refuses destinations containing the project's source files, skips unchanged
output, and removes obsolete published files. It does not change your configured remotes, local
branches, or remote-tracking refs, or switch your working checkout to the publication branch.
`just docs`, `just docs-serve`, and `just pages-build` still build your working tree for local
preview; only the publishing recipe uses an isolated committed-source build.

```sh
just pages-publish [remote] [branch]
```

The default remote is `origin` and default branch is **`pages`**, not `gh-pages`. Choose the
GitHub remote explicitly if `origin` points elsewhere. For example, if your GitHub remote is
named `github`:

```sh
just pages-publish github
# Or specify both the remote and publication branch:
just pages-publish github pages
```

Inspect `git remote -v` and use the intended existing remote name; do not assume `origin`
is the public GitHub repository. The public source repository is
<https://github.com/kryesh/skyhook>.

In that GitHub repository's **Settings → Pages**, choose **Deploy from a branch**, select
**`pages`** (or the branch explicitly chosen above), and select **`/ (root)`**. The recipe
publishes the contents of the built book at that branch's root. Publication and GitHub's deployment
must complete before the canonical site becomes available.

There is no new automatic workflow or assumed hosted-runner bootstrap. A CI runner with the
required tools already installed can invoke the same `just docs-check` and `just pages-build`
recipes; authenticated publishing can likewise reuse the explicit publishing recipe.

## Test publishing changes

Run the publishing regression tests without contacting a server or executing a real push:

```sh
bash scripts/test-pages-publish.sh
```

They use the installed mdBook and just with temporary local Git repositories. Coverage includes
initial publication, unchanged output, deleted pages, source-branch protection, dirty-checkout
rejection, fetch isolation, and cleanup after build or push failures.
