#!/usr/bin/env bash
# Offline integration test: a Git wrapper simulates pushes by locally fetching
# commits into a temporary bare repository. Never runs git push or contacts hosts.
set -euo pipefail
project=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
# Ignore user-level hooks, signing, and URL rewrites in this isolated fixture.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
real_git=$(command -v git)
real_mdbook=$(command -v mdbook)
sandbox=$(mktemp -d /tmp/skyhook-pages-test.XXXXXXXX)
trap 'rm -rf -- "$sandbox"' EXIT
export PAGES_TEST_GIT=$real_git PAGES_TEST_SOURCE="$sandbox/source" PAGES_TEST_REMOTE="$sandbox/remote.git" PAGES_TEST_LOG="$sandbox/push.log"
export PAGES_TEST_MDBOOK="$real_mdbook" PAGES_TEST_BUILD_LOG="$sandbox/build.log" PAGES_TEST_REMOTE_LOG="$sandbox/remote.log"
mkdir -p "$sandbox/bin" "$PAGES_TEST_SOURCE/scripts" "$PAGES_TEST_SOURCE/docs/src/assets"
cat >"$sandbox/bin/git" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
case ${1:-} in
    ls-remote|fetch|push) printf '%s\n' "$*" >>"$PAGES_TEST_REMOTE_LOG" ;;
esac
if [[ ${1:-} == push ]]; then
    printf '%s\n' "$*" >>"$PAGES_TEST_LOG"
    [[ $# == 4 && $2 == --no-follow-tags && $3 == origin && $4 == *:refs/heads/pages ]] || exit 99
    [[ ${PAGES_TEST_REJECT_PUSH:-0} != 1 ]] || { echo 'simulated push rejection' >&2; exit 1; }
    exec "$PAGES_TEST_GIT" --git-dir="$PAGES_TEST_REMOTE" fetch --no-tags --no-write-fetch-head "$PAGES_TEST_SOURCE" "$4"
fi
exec "$PAGES_TEST_GIT" "$@"
WRAPPER
cat >"$sandbox/bin/mdbook" <<'WRAPPER'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == build && $2 == docs ]]
[[ $PWD != "$PAGES_TEST_SOURCE" && -f .git ]]
# The build must run in a detached, pristine checkout of the captured source.
! "$PAGES_TEST_GIT" symbolic-ref --quiet HEAD
sha=$("$PAGES_TEST_GIT" rev-parse HEAD)
[[ $sha == "$("$PAGES_TEST_GIT" -C "$PAGES_TEST_SOURCE" rev-parse HEAD)" ]]
"$PAGES_TEST_GIT" diff --quiet HEAD
[[ ! -e docs/src/.env && ! -e docs/src/extra.html && ! -e docs/book/stale-secret.txt ]]
# A recipe/preprocessor side effect must never appear in the live source tree.
printf 'build side effect\n' > .build-side-effect
printf '%s\t%s\n' "$PWD" "$sha" >>"$PAGES_TEST_BUILD_LOG"
exec "$PAGES_TEST_MDBOOK" "$@"
WRAPPER
chmod +x "$sandbox/bin/git" "$sandbox/bin/mdbook"
export PATH="$sandbox/bin:$PATH"
git init --bare --quiet "$PAGES_TEST_REMOTE"
git init --quiet -b main "$PAGES_TEST_SOURCE"
cd "$PAGES_TEST_SOURCE"
git config commit.gpgsign false
git config user.name 'Pages Test'
git config user.email 'pages-test@example.invalid'
git remote add origin "$PAGES_TEST_REMOTE"
cp "$project/justfile" .
cp "$project/scripts/pages-publish.sh" scripts/
printf '/book/\n' > docs/.gitignore
printf '.env\n.env.*\n/docs/src/extra.html\n' > .gitignore
printf 'legitimate tracked asset\n' > docs/src/assets/tracked.txt
cat > docs/book.toml <<'BOOK'
[book]
title = "Pages Test"
src = "src"
[build]
build-dir = "book"
BOOK
printf '# Summary\n\n- [Home](index.md)\n- [Old page](old.md)\n' >docs/src/SUMMARY.md
printf '# Home\n\nOffline fixture.\n' >docs/src/index.md
printf '# Old page\n\nDelete this later.\n' >docs/src/old.md
printf 'source only\n' > README.md
git add .
git commit --quiet -m 'fixture'
source_sha=$(git rev-parse HEAD)
# Ignored live files must neither enter publication nor be altered by its build.
printf 'SKYHOOK_TEST_IGNORED_SECRET_ENV\n' > docs/src/.env
printf 'SKYHOOK_TEST_IGNORED_SECRET_HTML\n' > docs/src/extra.html
mkdir -p docs/book
printf 'SKYHOOK_TEST_IGNORED_SECRET_STALE\n' > docs/book/stale-secret.txt
printf 'old local preview\n' > docs/book/index.html
cp -a docs/book "$sandbox/live-book-before"
# Deliberately stale remote-tracking refs expose opportunistic fetch updates.
git update-ref refs/remotes/origin/pages "$source_sha"
git update-ref refs/remotes/origin/main "$source_sha"
tracking_refs=$(git for-each-ref --format='%(refname) %(objectname)' refs/remotes/)
assert_cleaned() {
    [[ $(git for-each-ref --format='%(refname) %(objectname)' refs/remotes/) == "$tracking_refs" ]]
    [[ $(git symbolic-ref --short HEAD) == main ]]
    [[ $(git worktree list --porcelain | grep -c '^worktree ') == 1 ]]
    [[ -z $(git for-each-ref refs/skyhook-pages) ]]
    [[ -z $(git status --porcelain) ]]
    [[ ! -e .build-side-effect ]]
    diff -r docs/book "$sandbox/live-book-before"
    [[ $(cat docs/src/.env) == SKYHOOK_TEST_IGNORED_SECRET_ENV ]]
    [[ $(cat docs/src/extra.html) == SKYHOOK_TEST_IGNORED_SECRET_HTML ]]
    # Every logged build directory must have been removed, even on failures.
    if [[ -f $PAGES_TEST_BUILD_LOG ]]; then
        while IFS=$'\t' read -r directory sha; do
            [[ ! -e $directory ]]
        done <"$PAGES_TEST_BUILD_LOG"
    fi
}
just pages-publish >"$sandbox/initial.log" 2>&1 || { cat "$sandbox/initial.log"; exit 1; }
first=$(git --git-dir="$PAGES_TEST_REMOTE" rev-parse pages)
[[ $(git --git-dir="$PAGES_TEST_REMOTE" rev-list --count pages) == 1 ]]
git --git-dir="$PAGES_TEST_REMOTE" cat-file -e pages:.nojekyll
git --git-dir="$PAGES_TEST_REMOTE" cat-file -e pages:old.html
! git --git-dir="$PAGES_TEST_REMOTE" cat-file -e pages:README.md 2>/dev/null
git --git-dir="$PAGES_TEST_REMOTE" log -1 --format=%B pages | grep -q "$source_sha"
assert_cleaned
printf 'PASS initial orphan publication, source SHA, artifact-only content, cleanup\n'
[[ $(cut -f2 "$PAGES_TEST_BUILD_LOG") == "$source_sha" ]]
[[ $(git --git-dir="$PAGES_TEST_REMOTE" show pages:assets/tracked.txt) == 'legitimate tracked asset' ]]
for excluded in .env extra.html stale-secret.txt; do
    ! git --git-dir="$PAGES_TEST_REMOTE" cat-file -e "pages:$excluded" 2>/dev/null
done
! git --git-dir="$PAGES_TEST_REMOTE" grep -q SKYHOOK_TEST_IGNORED_SECRET pages --
printf 'PASS exact committed build source, tracked assets retained, ignored secrets excluded, live files untouched\n'
# Direct invocation from outside the repository must still use isolated paths.
(cd "$sandbox"; bash "$PAGES_TEST_SOURCE/scripts/pages-publish.sh") >"$sandbox/unchanged.log" 2>&1
[[ $(git --git-dir="$PAGES_TEST_REMOTE" rev-parse pages) == "$first" ]]
[[ $(wc -l <"$PAGES_TEST_LOG") == 1 ]]
assert_cleaned
printf 'PASS unchanged output creates no commit or push; fetch preserves remote-tracking refs\n'
# Seed remote source branches without a push. Publication must reject these
# even when the checkout is on another branch or has a detached HEAD.
git --git-dir="$PAGES_TEST_REMOTE" fetch --quiet --no-tags --no-write-fetch-head "$PAGES_TEST_SOURCE" \
    "$source_sha:refs/heads/main" "$source_sha:refs/heads/source-archive"
for destination in main source-archive; do
    for checkout in feature detached; do
        if [[ $checkout == feature ]]; then
            git checkout --quiet -B test-feature "$source_sha"
        else
            git checkout --quiet --detach "$source_sha"
        fi
        before_head=$(git symbolic-ref --quiet HEAD || true)
        push_count=$(wc -l <"$PAGES_TEST_LOG")
        if just pages-publish origin "$destination" >"$sandbox/source-destination.log" 2>&1; then
            echo 'source-containing destination was accepted' >&2; exit 1
        fi
        grep -q 'refusing to publish over source-containing destination' "$sandbox/source-destination.log"
        [[ $(wc -l <"$PAGES_TEST_LOG") == "$push_count" ]]
        [[ $(git --git-dir="$PAGES_TEST_REMOTE" rev-parse "$destination") == "$source_sha" ]]
        [[ $(git rev-parse HEAD) == "$source_sha" ]]
        [[ $(git symbolic-ref --quiet HEAD || true) == "$before_head" ]]
        git checkout --quiet main
        assert_cleaned
        printf 'PASS source destination %s refused from %s checkout before push\n' "$destination" "$checkout"
    done
done
rm docs/src/old.md
printf '# Summary\n\n- [Home](index.md)\n' >docs/src/SUMMARY.md
git add --all; git commit --quiet -m 'delete old page'
just pages-publish >"$sandbox/delete.log" 2>&1
! git --git-dir="$PAGES_TEST_REMOTE" cat-file -e pages:old.html 2>/dev/null
[[ $(git --git-dir="$PAGES_TEST_REMOTE" rev-parse pages^) == "$first" ]]
assert_cleaned
printf 'PASS deletion sync and linear publication history\n'
for state in unstaged staged untracked; do
    case $state in
        unstaged) printf 'dirty\n' >>README.md ;;
        staged) printf 'dirty\n' >>README.md; git add README.md ;;
        untracked) printf 'dirty\n' >untracked.txt ;;
    esac
    if just pages-publish >"$sandbox/dirty.log" 2>&1; then echo 'dirty checkout was accepted' >&2; exit 1; fi
    grep -q 'commit or remove all tracked' "$sandbox/dirty.log"
    git reset --quiet HEAD -- README.md
    git checkout -- README.md
    rm -f untracked.txt
    assert_cleaned
    printf 'PASS rejection of %s source changes\n' "$state"
done
printf '\nNew output.\n' >>docs/src/index.md
git add docs/src/index.md; git commit --quiet -m 'new output'
before_failure=$(git --git-dir="$PAGES_TEST_REMOTE" rev-parse pages)
if PAGES_TEST_REJECT_PUSH=1 just pages-publish >"$sandbox/rejection.log" 2>&1; then exit 1; fi
grep -q 'simulated push rejection' "$sandbox/rejection.log"
[[ $(git --git-dir="$PAGES_TEST_REMOTE" rev-parse pages) == "$before_failure" ]]
assert_cleaned
printf 'PASS failed push cleans worktree and private ref, preserves source and remote branch\n'
printf 'invalid toml [\n' >docs/book.toml
git add docs/book.toml; git commit --quiet -m 'broken build'
push_count=$(wc -l <"$PAGES_TEST_LOG")
remote_count=$(wc -l <"$PAGES_TEST_REMOTE_LOG")
if just pages-publish >"$sandbox/build-failure.log" 2>&1; then exit 1; fi
[[ $(wc -l <"$PAGES_TEST_LOG") == "$push_count" ]]
[[ $(wc -l <"$PAGES_TEST_REMOTE_LOG") == "$remote_count" ]]
assert_cleaned
printf 'PASS build failure cannot publish stale output or access remote; isolated source worktree cleaned\n'
printf 'All offline publication tests passed (real just + mdbook; zero real pushes).\n'
