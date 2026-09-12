#!/usr/bin/env bash
# Publish only freshly built documentation. Usage: pages-publish.sh [remote [branch]]
# Requires installed just/mdbook and a configured Git identity. No tool installation.
set -euo pipefail

fail() { printf 'pages-publish: %s\n' "$*" >&2; exit 1; }
[[ $# -le 2 ]] || fail 'usage: pages-publish.sh [remote [branch]]'
remote=${1:-origin}
branch=${2:-pages}
[[ $remote != -* && $branch != -* ]] || fail 'remote and branch must not start with a dash'

# Resolve relative to the script, not the caller's working directory.
root=$(git -C "$(dirname -- "${BASH_SOURCE[0]}")/.." rev-parse --show-toplevel)
cd "$root"
git check-ref-format "refs/heads/$branch" >/dev/null || fail 'invalid publication branch'
git remote get-url -- "$remote" >/dev/null || fail "remote is not configured: $remote"
source_branch=$(git symbolic-ref --quiet --short HEAD || true)
[[ $source_branch != "$branch" ]] || fail 'refusing to publish over the checked-out source branch'
source_sha=$(git rev-parse --verify HEAD)

require_clean() {
    local status
    status=$(git status --porcelain=v1 --untracked-files=all --ignore-submodules=none)
    if [[ -n $status ]]; then
        printf '%s\n' "$status" >&2
        fail 'commit or remove all tracked, staged, and nonignored untracked source changes first'
    fi
    [[ $(git rev-parse HEAD) == "$source_sha" ]] || fail 'source HEAD changed during publication'
}

# Ignored local files are allowed, but must never become publication inputs.
# Build only committed sources in a fresh detached worktree; neither ignored
# docs/src assets nor stale live docs/book output are copied into that worktree.
require_clean
scratch=$(mktemp -d "${TMPDIR:-/tmp}/skyhook-pages.XXXXXXXX")
source_worktree="$scratch/source"
worktree="$scratch/output"
# Unique private ref: do not alter local branches or remote-tracking refs.
fetch_ref="refs/skyhook-pages/$(basename -- "$scratch")"
cleanup() {
    local status=$? path
    trap - EXIT
    for path in "$worktree" "$source_worktree"; do
        if [[ -e $path/.git ]]; then
            if ! git worktree remove --force "$path"; then
                printf 'pages-publish: could not remove worktree; retained %s for recovery\n' "$path" >&2
                status=1
            fi
        fi
    done
    git update-ref -d "$fetch_ref" || status=1
    if [[ ! -e $worktree/.git && ! -e $source_worktree/.git ]]; then
        rm -rf -- "$scratch" || status=1
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Explicit paths keep justfile lookup and recipe execution inside the committed
# source checkout, independently of the caller's working directory.
git worktree add --detach "$source_worktree" "$source_sha"
(
    cd "$source_worktree"
    just --justfile "$source_worktree/justfile" --working-directory "$source_worktree" pages-build
)
require_clean
book="$source_worktree/docs/book"
[[ -d $book && ! -L $book && -s $book/index.html ]] || fail 'expected a nonempty docs/book/index.html'
[[ ! -e $book/.git && ! -L $book/.git ]] || fail 'book output must not contain .git'
[[ -f $book/.nojekyll ]] || fail 'pages-build did not create .nojekyll'

# No remote inspection, fetch, or push occurs until the isolated build succeeds.
base=''
if git ls-remote --exit-code --heads "$remote" "refs/heads/$branch" >"$scratch/remote-head"; then
    git fetch --no-tags --no-write-fetch-head --refmap='' "$remote" "refs/heads/$branch:$fetch_ref"
    base=$(git rev-parse --verify "$fetch_ref^{commit}")
    # Branch names alone cannot distinguish source from generated output (the
    # caller may be on a feature branch or detached HEAD). Never append a Pages
    # commit to a destination that still contains this project's source files.
    for indicator in Cargo.toml justfile docs/book.toml; do
        if git cat-file -e "$base:$indicator" 2>/dev/null; then
            fail "refusing to publish over source-containing destination $remote/$branch ($indicator)"
        fi
    done
else
    status=$?
    [[ $status -eq 2 ]] || fail "could not inspect $remote/$branch"
fi
# For a new Pages branch, use the source only to provision a detached worktree.
# The publication commit below has no parent, so it contains no source history.
git worktree add --detach "$worktree" "${base:-$source_sha}"

# Mirror the artifact, including dotfiles and deletions, but preserve the worktree
# administration file. No rsync dependency and no changes to the source checkout.
find "$worktree" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf -- {} +
cp -a "$book/." "$worktree/"
git -C "$worktree" add --all --force
new_tree=$(git -C "$worktree" write-tree)
if [[ -n $base && $new_tree == "$(git rev-parse "$base^{tree}")" ]]; then
    printf 'Pages output is unchanged; nothing to commit or push.\n'
    exit 0
fi

# Recheck before the only remote write. A racing publisher is rejected by the
# ordinary non-force push, never overwritten.
require_clean
parents=()
[[ -z $base ]] || parents=(-p "$base")
commit=$(git -C "$worktree" commit-tree "$new_tree" "${parents[@]}" -m "Publish documentation from $source_sha")
git push --no-follow-tags "$remote" "$commit:refs/heads/$branch"
printf 'Published documentation from %s to %s/%s (%s).\n' "$source_sha" "$remote" "$branch" "$commit"
