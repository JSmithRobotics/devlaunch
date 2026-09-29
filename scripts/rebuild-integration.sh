#!/usr/bin/env bash
# Rebuild the fork's integration branch from upstream plus the topic branches.
#
# The fork keeps no long-lived branch of its own that anyone rebases. Each
# change the fork carries lives on a topic branch rooted on upstream, one
# concern each, and `integration` is the disposable merge of all of them. It is
# deleted and rebuilt by this script rather than merged forward, which is what
# lets a topic branch be sent upstream, rewritten after review, or dropped when
# it lands, without any of that touching the others.
#
# Three things follow, and they are the whole reason for the shape:
#
#   - A topic branch is always PR-able. It is rooted on upstream and contains
#     one concern, so `gh pr create` needs no extraction step.
#   - A merged topic costs nothing to retire. Delete its line from TOPICS below
#     and rebuild; upstream now carries it.
#   - Conflicts are resolved once. `rerere` (enabled below) records each
#     resolution and replays it on every later rebuild, so tracking a moving
#     upstream is re-running this rather than re-deciding.
#
# Never commit to `integration`. It is output. A change belongs on the topic
# branch it is a change to, and arrives here by rebuild.

set -euo pipefail

UPSTREAM="${1:-origin/main}"
# Namespaced under fork/ beside the topic branches, and not a bare
# "integration": an old integration/dl-web branch makes that a ref directory,
# so git refuses to create a ref of the same name.
INTEGRATION="${INTEGRATION_BRANCH:-fork/integration}"

# One concern each, all rooted on upstream. A branch whose PR has merged
# upstream comes OUT of this list; that is how the fork shrinks.
TOPICS=(
    fork/identity                       # not upstreamable: channel, recipe, release workflow
    feat/from-ref                       # PR #649
    feat/claude-profiles-json           # PR #650
    fix/agent-socket-private-directory  # PR #648
    fix/agent-socket-own-directory      # held, pending a repro on upstream main
)

say() { printf '\n=== %s\n' "$*"; }

git rev-parse --verify --quiet "$UPSTREAM" >/dev/null \
    || { echo "no such ref: $UPSTREAM" >&2; exit 1; }

# Replay previously recorded conflict resolutions rather than asking again.
git config rerere.enabled true

for topic in "${TOPICS[@]}"; do
    git rev-parse --verify --quiet "$topic" >/dev/null \
        || { echo "missing topic branch: $topic" >&2; exit 1; }
done

say "rebuilding $INTEGRATION from $UPSTREAM ($(git rev-parse --short "$UPSTREAM"))"

# -B moves the branch even if it exists. Refuse while it is checked out here,
# which would silently reset the working tree instead.
current="$(git symbolic-ref --quiet --short HEAD || true)"
if [ "$current" = "$INTEGRATION" ]; then
    echo "$INTEGRATION is checked out in this worktree; run from another one" >&2
    exit 1
fi
git branch -f "$INTEGRATION" "$UPSTREAM"

work="$(mktemp -d)"
trap 'git worktree remove --force "$work" 2>/dev/null || true; rm -rf "$work"' EXIT
git worktree add --quiet "$work" "$INTEGRATION"

for topic in "${TOPICS[@]}"; do
    say "merging $topic"
    if ! git -C "$work" merge --no-edit --no-ff "$topic"; then
        # rerere may have staged a resolution already; a clean index means it did.
        if git -C "$work" diff --check --quiet && \
           ! git -C "$work" ls-files --unmerged | grep -q .; then
            echo "rerere resolved it; committing"
            git -C "$work" commit --no-edit
        else
            echo >&2
            echo "CONFLICT merging $topic, and rerere had no recorded resolution." >&2
            echo "Resolve it in $work, 'git add' the files, 'git commit', then" >&2
            echo "re-run this script: rerere will have learned it for next time." >&2
            trap - EXIT
            exit 1
        fi
    fi
done

say "$INTEGRATION rebuilt: $(git rev-parse --short "$INTEGRATION")"
git -C "$work" log --oneline "$UPSTREAM..$INTEGRATION" | sed 's/^/    /'
