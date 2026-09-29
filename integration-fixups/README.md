# Integration fixups

Patches applied to `fork/integration` after every topic has merged, by
`scripts/rebuild-integration.sh`.

## Why this exists rather than a topic branch

These reconcile two topics that are each correct on their own and clash only
once merged. `feat/lxcfs-proc-view` adds a parameter to `up_args`;
`feat/claude-profile-mount` has tests that call `up_args`. Neither branch is
broken, neither can see the other, and neither can carry the fix: the parameter
does not exist in one, the test does not exist in the other.

A fixups *branch* does not work either. It would have to be based on the merge
to see both, and the merge is rebuilt with fresh SHAs every time, so such a
branch goes stale on the first rebuild and merging it drags a whole parallel
history back in.

A patch is the thing that survives, because it is expressed against content
rather than against history.

## The rule these patches live under

**A patch here is a failure to be removed, not a place to put things.** Every
one is a change that belongs upstream in a topic and cannot get there yet. When
a topic merges upstream, or when the two sides can finally see each other, the
patch goes.

Keep them minimal and mechanical. Anything needing judgement belongs on a topic
branch where it can be reviewed as part of the change that motivates it.

## When one stops applying

The rebuild stops and says so. That is the intended behaviour: a patch that no
longer applies is usually a topic having changed underneath it, and guessing at
a three-way merge of a reconciliation nobody reviewed is worse than stopping.
Re-derive it against the new merge and replace the file.
