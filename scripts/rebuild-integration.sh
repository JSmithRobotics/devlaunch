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
    feat/claude-profile-mount           # PR #652
    feat/lxcfs-proc-view                # to propose upstream
    fork/public-api-toolchain           # not upstreamable: upstream pins a nightly instead
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

# Refuse while the branch is checked out ANYWHERE, not merely here: git itself
# rejects a force-update of a branch held by any worktree, and the first version
# of this check only looked at the current one, so the script got as far as
# printing "rebuilding" before git refused underneath it. Ask git for the whole
# list instead of guessing.
held_by="$(git worktree list --porcelain \
    | awk -v want="refs/heads/$INTEGRATION" '
        /^worktree /  { wt = substr($0, 10) }
        /^branch /    { if (substr($0, 8) == want) print wt }
    ')"
if [ -n "$held_by" ]; then
    echo "$INTEGRATION is checked out at:" >&2
    echo "$held_by" | sed 's/^/    /' >&2
    echo "Remove that worktree, or check out something else there, and re-run." >&2
    exit 1
fi
git branch -f "$INTEGRATION" "$UPSTREAM"

work="$(mktemp -d)"
trap 'git worktree remove --force "$work" 2>/dev/null || true; rm -rf "$work"' EXIT
git worktree add --quiet "$work" "$INTEGRATION"

for topic in "${TOPICS[@]}"; do
    say "merging $topic"
    if ! git -C "$work" merge --no-edit --no-ff "$topic"; then
        # Ask rerere what is STILL unresolved, rather than reading the index.
        # rerere rewrites the working tree but leaves the path unmerged until it
        # is added, so `ls-files --unmerged` reports a file rerere has already
        # fixed and this script used to bail on a conflict it had just replayed.
        # `rerere remaining` is the question actually being asked.
        remaining="$(git -C "$work" rerere remaining 2>/dev/null || true)"
        if [ -z "$remaining" ] && [ -n "$(git -C "$work" ls-files --unmerged)" ]; then
            echo "rerere replayed every conflict; staging and committing"
            git -C "$work" ls-files --unmerged --format='%(path)' \
                | sort -u \
                | while IFS= read -r path; do git -C "$work" add -- "$path"; done
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

# --- fixups -----------------------------------------------------------------
# Reconciliations between topics that are each correct alone and clash only once
# merged. Expressed as patches rather than as a branch because the merge is
# rebuilt with fresh SHAs every time, so anything anchored to its history goes
# stale immediately; a patch is anchored to content and survives. See
# integration-fixups/README.md, including the rule that every patch here is a
# failure to be removed rather than a place to put things.
fixups_dir="$(git rev-parse --show-toplevel)/integration-fixups"
if [ -d "$fixups_dir" ]; then
    for patch in "$fixups_dir"/*.patch; do
        [ -e "$patch" ] || continue
        say "applying fixup $(basename "$patch")"
        if ! git -C "$work" apply --index "$patch"; then
            echo >&2
            echo "FIXUP FAILED: $(basename "$patch") no longer applies." >&2
            echo "That usually means a topic changed underneath it. Re-derive it" >&2
            echo "against the new merge and replace the file; do not guess at a" >&2
            echo "three-way merge of a reconciliation nobody reviewed." >&2
            trap - EXIT
            exit 1
        fi
    done
    if ! git -C "$work" diff --cached --quiet; then
        git -C "$work" commit --quiet -m "chore: integration fixups

Applied by scripts/rebuild-integration.sh from integration-fixups/. Each one
reconciles topics that are correct alone and clash only once merged; see that
directory's README for why they are patches and not a branch."
    fi
fi

say "$INTEGRATION rebuilt: $(git rev-parse --short "$INTEGRATION")"
git -C "$work" log --oneline "$UPSTREAM..$INTEGRATION" | sed 's/^/    /'
