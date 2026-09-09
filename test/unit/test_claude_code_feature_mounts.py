"""The protection the claude-code feature documents, held against what it mounts.

The feature's README describes a read-write bind of `~/.claude` with the
subdirectories holding *executable instructions* mounted read-only on top of it,
plus `~/.agents/skills` outside it for the skills Claude and Codex share, and
gives the reason: those are files a prompt injection that edits one of them is
not confined by. The edit is on the host, and it runs again in every later session,
in every other container, on the developer's own machine.

Two failures have to be prevented here, and they pull in opposite directions.

**A protection that is documented and absent.** The manifest once mounted the
whole configuration directory read-write while the README described granular
read-only mounts ([#108](https://github.com/blooop/devlaunch/issues/108)), so
none of it was true. The assertions below are written as *agreement* between the
README and the manifest rather than as a list of paths kept here: a third copy of
the list is a third thing to forget, and a path added to one side and not the
other has to fail.

**A protection that is real on the day it is written and gone by Tuesday.** This
is the one that is not obvious from reading a manifest, and it is why
`test_every_mount_source_is_a_directory` exists. A bind mount of a *file* does
not survive its source being replaced by rename -- the mount hangs off the
dentry, the rename puts a new one at that name, and the mount leaves the
namespace. Under the read-write parent this feature needs, a read-only file
mount is therefore enforced only until the developer next edits the file, after
which the path falls through to the writable parent and `docker inspect` still
lists a mount that is no longer there.

That is measured rather than reasoned about, both ways round, because the
direction of the failure follows the parent and neither direction is safe:
a read-write parent leaves the file writable, a read-only parent leaves it
read-only and breaks a token refresh. A mount of a *directory* survives the same
rename intact. So the rule the manifest is held to is not "these paths are
read-only" but "every source is a directory", which is the form of the rule that
cannot rot -- and the tempting change it rejects is naming `CLAUDE.md` and
`settings.json` in the mount list, which passes review, appears in `docker
inspect`, and stops being true on the next edit.

The same rename is what the read-write directory mount is *for*. A file mount
pinned the inode that existed when the container was created, so an account
switch on the host reached no running container: it went on authenticating as the
account the developer had left. Only a directory mount resolves names per access.

What is asserted here is the *text* of the manifest. Whether a devcontainer
Feature's `readonly` mount really reaches Docker as a read-only bind, and whether
it survives the rename, are claims about a running container, and are settled in
the e2e suite, not here.
"""

import json
import os
import re
from pathlib import Path

import pytest

from fixtures.markdown_sections import section as _section
from unit.test_devcontainer_manifest import (
    parse_mount,
    run_initialize_command,
    strip_jsonc_comments,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
FEATURE_DIR = REPO_ROOT / ".devcontainer" / "claude-code"
FEATURE_JSON = FEATURE_DIR / "devcontainer-feature.json"
FEATURE_README = FEATURE_DIR / "README.md"
DEVCONTAINER_JSON = REPO_ROOT / ".devcontainer" / "devcontainer.json"

LOCAL_HOME = "${localEnv:HOME}"
CONFIG_DIRNAME = ".claude"
HOST_CONFIG_DIR = f"{LOCAL_HOME}/{CONFIG_DIRNAME}"

READ_ONLY_HEADING = "### Read-Only Mounts (Security-Protected)"
READ_WRITE_HEADING = "### Read-Write Mounts (Authentication & State)"

# `~/.claude/agents/` and `~/.claude/CLAUDE.md` differ by one character, and that
# character is the whole difference between a directory the pre-create hook has
# to `mkdir` and a file it must not mount. The README's trailing slash is
# therefore read as a declaration rather than as typography.
DOCUMENTED_PATH = re.compile(r"^- `~/(?P<path>[^`]+)`")


def documented_home_paths(heading: str) -> set:
    """The home-relative paths the README lists under one mount heading.

    Only the leading code span of a bullet counts. Prose under the heading
    mentions these files too, and a test that matched anywhere in the section
    would be satisfied by a sentence *about* a mount that no longer exists.

    Home-relative and not `~/.claude`-relative, for every caller, because the
    feature now mounts outside the configuration directory. A second accessor
    returning the `.claude/` subset was the same `set` of the same `str` under a
    different name, so the base each one was relative to lived only in that
    name -- and it silently dropped `~/.agents/skills/`, which is how
    `mounted_files` came to promise a derivation it no longer performed.
    """
    paths = {
        match.group("path")
        for match in (
            DOCUMENTED_PATH.match(line) for line in _section(FEATURE_README, heading).splitlines()
        )
        if match
    }
    assert paths, f"the README lists no mounts under {heading!r}"
    escaping = sorted(path for path in paths if path.startswith("/"))
    assert not escaping, (
        f"{escaping} under {heading!r} came out absolute, and every caller joins these onto a "
        f"home: `scratch / '/.claude/x'` is '/.claude/x', so the path leaves the scratch home "
        f"for the developer's real one"
    )
    return paths


@pytest.fixture(name="feature")
def feature_fixture() -> dict:
    return json.loads(FEATURE_JSON.read_text())


@pytest.fixture(name="mounts")
def mounts_fixture(feature) -> list:
    return [parse_mount(spec) for spec in feature["mounts"]]


@pytest.fixture(name="host_home")
def host_home_fixture(tmp_path) -> Path:
    """A host home as the pre-create hook leaves it, on a fresh machine.

    Every test that asks what *kind* of thing a mount source is needs a host to
    look at, and this is the only honest one to use: the hook is what creates
    these paths on a machine that has never run Claude, so the answer it gives is
    the answer Docker will get.

    The home and not the `~/.claude` inside it, because the feature now mounts
    `~/.agents/skills` too and a fixture returning the child left every caller
    walking back up out of it.
    """
    devcontainer = json.loads(strip_jsonc_comments(DEVCONTAINER_JSON.read_text()))
    result = run_initialize_command(devcontainer, tmp_path)
    assert result.returncode == 0, result.stderr
    return tmp_path


def resolve(source: str, host_home: Path) -> Path:
    """A manifest mount source as a path under the test's scratch home.

    The assert is the whole of what this adds over a `removeprefix`, and it is
    what keeps a wrong answer from being a plausible one: a source that is not
    under `${localEnv:HOME}` has no place under the scratch home either, and
    silently resolving it to one would check a path the real mount never names.
    Which host paths may be mounted at all is a separate question, asked by
    `test_no_mount_reaches_a_host_path_the_readme_does_not_list`.
    """
    prefix = f"{LOCAL_HOME}/"
    assert source.startswith(prefix), f"{source} is outside the host home"
    return host_home / source.removeprefix(prefix)


def nested_sources(mounts: list) -> dict:
    """Each mount *inside* the configuration directory, keyed by relative path.

    The bind of the configuration directory itself is excluded, because every
    rule below says something different about it: it is the one mount that is
    supposed to be writable, and the one whose relative path is empty.
    """
    granular = {
        mount["source"][len(f"{HOST_CONFIG_DIR}/") :]: mount
        for mount in mounts
        if mount.get("source", "").startswith(f"{HOST_CONFIG_DIR}/")
    }
    assert granular, "the feature mounts nothing under the host's configuration directory"
    return granular


def config_dir_mount(mounts: list) -> dict:
    """The bind of `~/.claude` itself."""
    for mount in mounts:
        if mount.get("source") == HOST_CONFIG_DIR:
            return mount
    raise AssertionError(
        "the feature does not mount the configuration directory itself, so the files "
        "reached through it -- credentials above all -- are not mounted at all"
    )


def test_every_mount_is_a_bind_of_a_host_path(mounts):
    """Each entry binds a real host path, rather than a volume of its own.

    A `type=volume` entry at the same target would leave the container with a
    plausible-looking, empty configuration and no connection to the host at all,
    which is a mount list that passes every other test in this file: the source
    still names a directory that exists, and the read-only flags still line up
    with the README.
    """
    assert mounts, "the feature mounts nothing"
    for mount in mounts:
        assert mount.get("type") == "bind", f"{mount.get('target')} is not a bind mount"


def test_every_mount_source_is_a_directory(mounts, host_home):
    """No mount names a file, whatever flags it would carry.

    This is the rule that keeps the read-only list honest, and it is stated over
    *all* mounts rather than only the read-only ones because both directions of
    the file-mount failure are bugs. A read-only file mount under this feature's
    read-write parent is a protection that ends at the developer's next edit; a
    read-write file mount is how the container came to hold a dead inode and go on
    authenticating as an account the developer had already left.

    Asserted against a host the pre-create hook has just set up, so "is a
    directory" is measured rather than inferred from the path's spelling.
    """
    for mount in mounts:
        source = resolve(mount["source"], host_home)
        assert source.is_dir(), (
            f"{mount['source']} is mounted but is not a directory. A bind mount of a file "
            f"does not survive its source being replaced by rename: the mount leaves the "
            f"namespace and the path falls through to the parent mount, taking any "
            f"'readonly' with it. Mount the directory that contains it instead."
        )


def test_the_configuration_directory_is_mounted_read_write(mounts):
    """`~/.claude` itself is bound, and is not read-only.

    Two claims in one, because each fails differently. Without the mount, the
    files that have no mount of their own -- `.credentials.json`, `.claude.json` --
    are not in the container at all, and Claude comes up logged out. With it
    read-only, they are there and current but a token refresh cannot write them,
    which is a login prompt on every launch.
    """
    assert "readonly" not in config_dir_mount(mounts)


def test_the_paths_documented_as_protected_are_exactly_the_read_only_mounts(mounts):
    """The README's read-only list and the manifest's read-only mounts agree.

    Set equality in both directions, because the two failures are equally bad and
    look nothing alike: a documented protection with no mount behind it is the
    defect this ticket reports, and an undocumented read-only mount is a file the
    container cannot write for reasons nobody wrote down.
    """
    read_only = {
        mount["source"].removeprefix(f"{LOCAL_HOME}/") for mount in mounts if "readonly" in mount
    }
    documented = {path.rstrip("/") for path in documented_home_paths(READ_ONLY_HEADING)}
    assert read_only == documented


def test_no_mount_reaches_a_host_path_the_readme_does_not_list(mounts):
    """Every mount is a documented read-only one, or the configuration bind itself.

    The test above only inspects mounts that carry `readonly`, so a mount that
    carries no flag at all is invisible to it. That was harmless while `resolve`
    refused any source outside `~/.claude`, because the only place an
    undocumented mount could land was under a parent that was already writable.
    Sharing `~/.agents/skills` widened `resolve` to the whole host home and took
    that limit away with it: a writable bind of any home directory -- `~/.codex`,
    `~/.ssh` -- now satisfies every other rule in this file, because it is a
    bind, its source is a directory the hook creates, and it is nested inside
    nothing.
    """
    sources = {
        mount["source"].removeprefix(f"{LOCAL_HOME}/")
        for mount in mounts
        if mount["source"] != HOST_CONFIG_DIR
    }
    documented = {path.rstrip("/") for path in documented_home_paths(READ_ONLY_HEADING)}
    assert sources == documented, (
        f"{sorted(sources - documented)} are mounted into the container from the host "
        f"home and appear under no mount heading in the README"
    )


def test_every_mount_of_a_protected_source_carries_readonly(mounts):
    """A protected source is read-only at *every* target it is mounted at.

    The rules above are set equality and dict lookup, and a source is what they
    key on, so a *second* mount of an already-protected source is absorbed by
    all of them: the set already contains it, and the dict keeps whichever entry
    came last. Docker creates both. One line --
    `source=${localEnv:HOME}/.agents/skills,target=/home/vscode/.agents/skills-rw,type=bind`
    -- hands the container write access to the host's shared skill bodies at a
    second path while every assertion in this file stays green, which is the
    protection this feature exists for, defeated by an entry that reads like a
    typo.
    """
    documented = {path.rstrip("/") for path in documented_home_paths(READ_ONLY_HEADING)}
    writable = sorted(
        mount["target"]
        for mount in mounts
        if mount["source"].removeprefix(f"{LOCAL_HOME}/") in documented and "readonly" not in mount
    )
    assert not writable, (
        f"{writable} mount a documented read-only source without `readonly`, so the host "
        f"path is writable from the container through those targets"
    )


def test_no_host_path_is_mounted_twice(mounts):
    """One source, one target, so nothing can be absorbed by keying on it.

    The narrower rule above catches the case that costs a protection. This is
    the general one, and it is what makes the set- and dict-shaped assertions in
    this file sound rather than accidentally sound.
    """
    sources = [mount["source"] for mount in mounts]
    repeated = sorted({source for source in sources if sources.count(source) > 1})
    assert not repeated, (
        f"{repeated} are each mounted more than once, and every other rule here keys on the "
        f"source, so the duplicate is invisible to them while Docker creates it"
    )


TROUBLESHOOTING = FEATURE_DIR / "TROUBLESHOOTING.md"


# Both documents draw their trees *inside* `~/.claude`, so a leading `~/` is the
# only thing marking a path as home-relative: `skills/` is the configuration
# directory's, `~/.agents/skills/` is not.
def as_mount_source(path: str) -> str:
    path = path.rstrip("/")
    return path.removeprefix("~/") if path.startswith("~/") else f"{CONFIG_DIRNAME}/{path}"


# A drawn directory entry, with or without box-drawing lead-in and with or
# without a trailing comment. Deliberately not keyed on `(read-only mount)`:
# the README draws the same seven paths with comments that say `# Custom
# agents`, so a pattern requiring the annotation read that copy as empty and
# then compared it against nothing.
TREE_ENTRY = re.compile(r"^[│├└─ ]*(?P<path>[~.\w][\w./-]*/)(?:\s+#.*)?$")
CODE_SPAN = re.compile(r"`([^`]+)`")
READ_ONLY_SYMPTOM = "→ Read-only"

# The tree under this heading is rooted at `~/.claude/` and names it, so the
# configuration bind is a legitimate entry there and the seven read-only mounts
# are the rest.
HOST_LAYOUT_HEADING = "### Host Machine"

# The two places the README hands over a `mkdir` to run. Anchored per section
# because it offers others -- a dotfiles installer example, a three-directory
# fragment -- and only these two claim to create what the feature mounts.
MKDIR_HEADINGS = ("### Host Machine", "### `bind mount source path does not exist`")


def brace_expanded(word: str) -> set:
    """`~/.claude/{agents,hooks}` as the two paths a shell would create."""
    head, brace, rest = word.partition("{")
    if not brace:
        return {word}
    names, _, tail = rest.partition("}")
    return {f"{head}{name}{tail}" for name in names.split(",")}


def documented_mkdir(heading: str) -> set:
    for line in _section(FEATURE_README, heading).splitlines():
        if line.startswith("mkdir -p "):
            return {
                as_mount_source(path)
                for word in line.removeprefix("mkdir -p ").split()
                for path in brace_expanded(word)
            }
    raise AssertionError(f"the README section {heading!r} no longer offers a by-hand mkdir")


def drawn_tree(lines: list) -> set:
    """The mount sources an ASCII directory tree draws, from its entry lines.

    Both trees mix roots: they are drawn inside `~/.claude` and then name
    `~/.agents/skills/` at the same indent, so the tilde is what says which
    base an entry is relative to. Both also draw `~/.claude/` itself as the
    root they hang from, which is the one bind that is supposed to be writable
    and is asserted elsewhere, so it is not part of what these copies promise.
    """
    paths = {
        as_mount_source(match.group("path"))
        for match in (TREE_ENTRY.match(line) for line in lines)
        if match
    }
    assert paths, "no tree entry was recognised, so this compares nothing"
    return paths - {CONFIG_DIRNAME}


def test_every_hand_written_copy_of_the_mount_list_says_the_same_thing(mounts):
    """The trees, the symptom list and the by-hand `mkdir`s agree with the manifest.

    Five hand-maintained copies of one list, and this repo's rule is that a
    second copy is allowed only where a test diffs it against the first. Only
    the README's Read-Only Mounts bullets had one, so the rest could and did
    drift: the `bind mount source path does not exist` remedy still created
    three directories of seven, which is a documented fix for a refused
    container create that leaves the create refused, and told the developer to
    truncate their own `settings.json` on the way past.
    """
    read_only = {
        mount["source"].removeprefix(f"{LOCAL_HOME}/") for mount in mounts if "readonly" in mount
    }
    troubleshooting = TROUBLESHOOTING.read_text().splitlines()
    copies = {
        "TROUBLESHOOTING.md's tree": drawn_tree(troubleshooting),
        f"the README's {HOST_LAYOUT_HEADING!r} tree": drawn_tree(
            _section(FEATURE_README, HOST_LAYOUT_HEADING).splitlines()
        ),
        "TROUBLESHOOTING.md's read-only symptom": {
            as_mount_source(path)
            for line in troubleshooting
            if READ_ONLY_SYMPTOM in line
            for path in CODE_SPAN.findall(line)
        },
        **{
            f"the README's {heading!r} mkdir": documented_mkdir(heading)
            for heading in MKDIR_HEADINGS
        },
    }
    for where, listed in copies.items():
        assert listed == read_only, (
            f"{where} disagrees with the manifest: {sorted(listed ^ read_only)} appears in one "
            f"and not the other"
        )


def test_nothing_nested_inside_the_configuration_directory_is_writable(mounts):
    """Every mount over the directory mount is read-only.

    A read-write mount nested inside a read-write parent buys nothing -- the path
    is already writable through the parent -- and costs the thing the parent was
    chosen for, because it is a file mount again and goes stale on the first
    rename. So the only reason to nest is to take write access away, and a nested
    mount that does not is a mistake with no upside to weigh it against.
    """
    writable = {path for path, mount in nested_sources(mounts).items() if "readonly" not in mount}
    assert not writable, (
        f"{sorted(writable)} are mounted read-write inside a read-write mount, which only "
        f"pins them to a stale inode"
    )


def test_the_paths_documented_as_writable_have_no_mount_of_their_own(mounts):
    """The writable state files are reached through the directory, not bound directly.

    This is the regression in its original form, asserted from the README's side.
    Both files were mounted individually and read-write, which is how a container
    ended up reading the inode that existed when it was created: an account switch
    on the host changed the file by rename, and the container never saw it.

    They still have to be *documented* as writable -- the README argues for each,
    token refresh and onboarding state, and that argument is what a reviewer
    weighs -- so the check is that the argument survives while the mount does not.
    """
    documented = {path.rstrip("/") for path in documented_home_paths(READ_WRITE_HEADING)}
    assert documented, "the README no longer says which files must stay writable"
    mounted = {f"{CONFIG_DIRNAME}/{path}" for path in nested_sources(mounts)}
    assert not (documented & mounted), (
        f"{sorted(documented & mounted)} are documented as writable and mounted individually; "
        f"a file mount pins the inode, so the container stops seeing host changes to them"
    )


def test_each_mount_lands_where_the_feature_tells_claude_to_look(feature, mounts):
    """Every host path arrives at the same relative place under `CLAUDE_CONFIG_DIR`.

    The feature sets that variable and mounts these paths, and nothing else
    checks that the two agree. They can disagree quietly: a mount whose target
    drifts leaves Claude reading a path that exists, is empty, and is nobody's
    configuration -- a container that comes up fine and behaves as though the
    host had never been set up.
    """
    config_dir = feature["containerEnv"]["CLAUDE_CONFIG_DIR"]
    assert config_dir_mount(mounts)["target"] == config_dir
    for relative, mount in nested_sources(mounts).items():
        assert mount.get("target") == f"{config_dir}/{relative}"


def test_the_pre_create_hook_creates_every_host_path_the_feature_mounts(mounts, host_home):
    """The mounted paths exist on the host before the container is asked to start.

    A missing bind source is not a degraded container: the create is refused
    outright with `bind mount source path does not exist`, measured on devpod
    0.26.1.

    The hook is read out of this repo's devcontainer manifest rather than named
    here, so repointing or deleting it fails this test instead of leaving it
    passing about a script nothing runs. A Feature cannot declare a host-side
    hook of its own; wiring this one up is the consuming devcontainer's job, and
    this is where that stays true.
    """
    for mount in mounts:
        source = resolve(mount["source"], host_home)
        assert source.exists(), f"{mount['source']} is mounted but the hook does not create it"


def test_the_pre_create_hook_creates_no_file_the_feature_does_not_mount(host_home):
    """It seeds no `{}` placeholders for paths nothing binds any more.

    The empty `.credentials.json` and `.claude.json` this used to write existed
    only to satisfy a bind source. With the files no longer mounted individually,
    a missing one cannot refuse the create, and Claude writes each itself on first
    use -- while an empty credentials file on a host that has never run Claude is
    indistinguishable from a logged-out session.
    """
    stray = [path.name for path in (host_home / CONFIG_DIRNAME).iterdir() if path.is_file()]
    assert not stray, f"the hook creates {sorted(stray)}, which nothing mounts"


def test_the_pre_create_hook_leaves_a_configuration_that_already_exists_alone(mounts, tmp_path):
    """It creates what is missing and writes to nothing else, mtimes included.

    Two reasons, and the second is what makes this a container-starting concern
    rather than good manners. The developer's real configuration is on the other
    end of these mounts, and a hook that rewrites it is a hook that eats
    somebody's setup. And this hook has to survive running *inside* a container it
    built -- which is the whole point of giving that container a Docker daemon --
    where the instruction directories are the read-only mounts, so a write fails
    with EROFS, and a non-zero pre-create hook aborts `devpod up` rather than
    degrading it.

    Modification times are the assertion because a read-only filesystem is not
    something a unit test can conjure, while "wrote to a path that was already
    there" is exactly what fails on one and is observable anywhere.
    """
    devcontainer = json.loads(strip_jsonc_comments(DEVCONTAINER_JSON.read_text()))
    run_initialize_command(devcontainer, tmp_path)

    existing = [resolve(mount["source"], tmp_path) for mount in mounts if "readonly" in mount]
    assert existing, "no configuration was created, so this asserts nothing"
    for path in existing:
        os.utime(path, ns=(0, 0))

    result = run_initialize_command(devcontainer, tmp_path)
    assert result.returncode == 0, result.stderr
    for path in existing:
        assert path.stat().st_mtime_ns == 0, f"the hook rewrote {path.name}"


# The two files that are writable and that a reader would most reasonably assume
# are not: `settings.json` can name a hook command inline, so a container writing
# one runs a command on the host, and `CLAUDE.md` is the instruction file a prompt
# injection would target. Both were mounted read-only under the old file-mount
# layout, and #362 moved to a directory mount without rewriting every section that
# said so.
WRITABLE_BUT_LOOKS_PROTECTED = ("CLAUDE.md", "settings.json")

# A heading is making a protection claim if it says so. `### What's Protected
# (Read-Only Mounts)` is the one that went stale: it sat 450 lines below the
# heading the mount-agreement test binds, kept claiming `CLAUDE.md` and
# `settings.json` were read-only for two releases after they stopped being, and
# was contradicted by this same file's own "What Is Not Mounted Read-Only".
PROTECTION_WORDS = ("read-only", "protected")


def _protection_headings(readme: str) -> list:
    return [
        line
        for line in readme.splitlines()
        if line.startswith("#") and any(word in line.lower() for word in PROTECTION_WORDS)
    ]


@pytest.mark.parametrize("document", [FEATURE_README, TROUBLESHOOTING], ids=lambda p: p.name)
def test_no_heading_claims_protection_for_a_writable_file(document):
    """No section listing protected paths may name a file that is writable.

    The mount-agreement test binds one heading by its exact text, so a *second*
    list of protected paths — which is what these documents grew — is checked by
    nothing. This asks the question of every heading that claims protection
    instead of one, because the failure was a heading nobody had registered.

    Both documents, because scoping it to the README is how TROUBLESHOOTING.md
    went on promising that `CLAUDE.md` and `settings.json` were read-only after
    they stopped being. That one is the worse of the two to get wrong: it is
    the page a developer opens when a write has just been refused.

    Only bullets count, for `documented_home_paths`'s reason: the prose under
    these headings discusses `settings.json` precisely to say it is *not*
    protected, and a substring match anywhere in the section would fail on the
    sentence that fixes the problem.
    """
    headings = _protection_headings(document.read_text())
    assert headings, f"no heading in {document.name} claims protection; the guard guards nothing"

    offences = []
    for heading in headings:
        for line in _section(document, heading).splitlines():
            stripped = line.lstrip()
            if not stripped.startswith(("-", "*")):
                continue
            for name in WRITABLE_BUT_LOOKS_PROTECTED:
                if name in stripped:
                    offences.append(f"{heading!r} lists {name}: {stripped.strip()}")

    assert not offences, (
        "a section claiming protection names a file that is writable from the "
        "container:\n  " + "\n  ".join(offences)
    )
