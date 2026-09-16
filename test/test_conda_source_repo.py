"""The recipe's source repo is configurable, and the guard for why it must stay so.

Before this, `conda.recipe/recipe.yaml`'s `source.git` was a bare literal
pointing at `https://github.com/blooop/devlaunch.git`. On a fork that is a
defect rather than a convenience: a conda build fetches UPSTREAM's source,
compiles upstream's code, and (given `PIXI_TOKEN` and a channel) publishes it
to the FORK's own channel under the fork's name -- succeeding green the whole
way, with nothing in the log to say whose code shipped.

The fix has two halves, and they only work together. The recipe reads a
`DEVLAUNCH_SOURCE_REPO` environment variable with upstream's URL as the
default (so a bare render, or upstream's own CI, behaves exactly as before);
`.github/workflows/conda-publish.yml`'s build step sets that variable to
`https://github.com/${{ github.repository }}`, which is upstream's own
literal when `github.repository` is `blooop/devlaunch` and the fork's when it
is not. A recipe that reads the variable but a workflow that never sets it
would be silently back to today's bug the moment the default drifted from
`github.repository`'s value for some run; a workflow that sets a variable no
recipe reads is dead configuration. Both are asserted below.

These are text assertions on the recipe and the workflow, in the same spirit
as `test_bench_workflow.py` and `test_ci_workflow.py`: there is no YAML/jinja
parser among this project's dependencies, and rendering the recipe end to end
belongs to `pixi exec rattler-build --render-only` (documented, not run here),
not to the unit suite.
"""

from pathlib import Path

ROOT = Path(__file__).parent.parent
RECIPE = ROOT / "conda.recipe" / "recipe.yaml"
CONDA_PUBLISH = ROOT / ".github" / "workflows" / "conda-publish.yml"
CARGO_TOML = ROOT / "rust" / "Cargo.toml"

# Without the `.git` suffix, which is `source.git`'s to append. That is also
# exactly the spelling `rust/Cargo.toml`'s `repository` key uses, which is what
# lets the two copies below be diffed against each other rather than merely
# both existing.
UPSTREAM_URL = "https://github.com/blooop/devlaunch"
ENV_VAR = "DEVLAUNCH_SOURCE_REPO"
# The context variable `source.git` must resolve through. Named here because two
# tests below assert on it and a rename should break them together.
CONTEXT_VAR = "repo_url"


def recipe_text() -> str:
    assert RECIPE.is_file(), f"{RECIPE} is the recipe this whole module is about"
    return RECIPE.read_text(encoding="utf-8")


def conda_publish_text() -> str:
    assert CONDA_PUBLISH.is_file(), f"{CONDA_PUBLISH} is the workflow that builds the recipe"
    return CONDA_PUBLISH.read_text(encoding="utf-8")


def cargo_toml_text() -> str:
    assert CARGO_TOML.is_file(), f"{CARGO_TOML} carries the other copy of upstream's URL"
    return CARGO_TOML.read_text(encoding="utf-8")


class TestTheRecipeDoesNotHardcodeARepo:
    """The defect this whole change is about: a bare literal in `source.git`
    means every fork's build fetches upstream's code, no matter whose channel
    it publishes to."""

    def test_source_git_resolves_through_the_env_var_rather_than_a_literal(self):
        """The regression this guards: `source: git:` naming
        `github.com/blooop/devlaunch.git` directly again, which silently
        brings back the fork-fetches-upstream defect regardless of what
        `conda-publish.yml` sets."""
        text = recipe_text()
        assert ENV_VAR in text, (
            f"{RECIPE.name} no longer reads {ENV_VAR}; a fork's build would silently "
            "fetch upstream's source again"
        )
        # The literal upstream URL may still appear -- it is the documented
        # default -- but only as the argument to the lookup, never standing
        # alone as `source.git`'s value.
        source_git_lines = [
            line
            for line in text.splitlines()
            if line.strip().startswith("git:") and "source" not in line
        ]
        # Naming the context variable, not merely "some jinja". A review of an
        # earlier version of this test reintroduced the whole defect as
        # `git: ${{ "https://github.com/blooop/devlaunch.git" }}` -- a literal
        # wearing an expression's clothes, which every fork would have fetched
        # while all eight tests here passed. Asserting on `${{` alone is a
        # syntax check; the fact being guarded is which value is used.
        for line in source_git_lines:
            assert CONTEXT_VAR in line, (
                f"source.git no longer resolves through `{CONTEXT_VAR}`, so "
                f"{ENV_VAR} cannot reach it: {line!r}"
            )

    def test_the_default_is_upstream_so_an_unset_render_is_unchanged(self):
        """Unset, this has to resolve to exactly what the recipe named before
        this change -- a developer running rattler-build with no environment
        set up, or a lookup that forgets to configure it, must still build
        upstream's code, not fail and not silently build something else."""
        assert UPSTREAM_URL in recipe_text()

    def test_the_about_urls_derive_from_the_same_variable(self):
        """`about.homepage`, `about.documentation` and `about.repository` are
        second and third and fourth copies of the same URL `source.git` uses.
        Left as literals they would describe upstream even on a fork whose
        source var is set correctly -- a smaller version of the same defect,
        in the metadata instead of the build."""
        text = recipe_text()
        for key in ("homepage:", "documentation:", "repository:"):
            line = next(row for row in text.splitlines() if row.strip().startswith(key))
            # `CONTEXT_VAR` and not merely `${{`, for the reason
            # `test_source_git_resolves_through_the_env_var_rather_than_a_literal`
            # spells out: `${{ "https://github.com/blooop/devlaunch" }}` is a
            # literal in an expression's clothes and satisfies a check for jinja.
            assert CONTEXT_VAR in line, (
                f"about.{key} no longer resolves through `{CONTEXT_VAR}`, so it "
                f"names one repository while the build fetches another: {line!r}"
            )

    def test_the_recipe_maintainer_is_left_alone_and_explained(self):
        """`extra.recipe-maintainers` names a person, not a publish location --
        upstream's own recipe already lists itself here, and a fork gains
        nothing by rewriting it. The comment is what stops a future edit from
        "fixing" this alongside the URLs above on the mistaken belief that
        every `blooop` literal in the file is the same kind of thing."""
        text = recipe_text()
        assert "- blooop" in text
        maintainers_idx = text.index("recipe-maintainers:")
        # A comment explaining the exception has to sit between the section
        # header and the literal it is defending.
        between = text[maintainers_idx : text.index("- blooop", maintainers_idx)]
        assert "#" in between, "the maintainers exception is undocumented"


class TestCondaPublishActuallySetsTheVariable:
    """The other half of the mechanism. A recipe that reads
    `DEVLAUNCH_SOURCE_REPO` but a workflow that never sets it on the build step
    is the same defect wearing a costume: every real build still resolves the
    recipe's default, which is upstream, regardless of whose fork is running
    the workflow."""

    def test_the_build_step_sets_the_source_repo_from_github_repository(self):
        text = conda_publish_text()
        assert ENV_VAR in text, f"{CONDA_PUBLISH.name} never sets {ENV_VAR}"
        # It has to be set from `github.repository`, which is automatically
        # correct for a fork with zero configuration -- not from a literal,
        # which would just move the hardcoded identity from the recipe to the
        # workflow.
        lines = [line for line in text.splitlines() if ENV_VAR in line and ":" in line]
        setter = [line for line in lines if "github.repository" in line]
        assert setter, f"{ENV_VAR} is mentioned but never set from `github.repository`: {lines!r}"

    def test_it_is_set_on_the_build_step_and_not_left_for_publish(self):
        """Setting it on the `publish` job's upload step would be too late:
        rattler-build already fetched the source in `build`. The env var has
        to be visible to the step that actually invokes rattler-build."""
        text = conda_publish_text()
        build_step_idx = text.index("Build conda package")
        env_idx = text.index(ENV_VAR)
        publish_job_idx = text.index("Publish to prefix.dev")
        assert build_step_idx < env_idx < publish_job_idx, (
            f"{ENV_VAR} is not set on the build step where rattler-build runs"
        )

    def test_the_build_dependency_channel_is_left_alone(self):
        """`--channel https://prefix.dev/blooop` resolves build DEPENDENCIES
        (the compilers) from where they are maintained -- a different thing
        from where the source comes from or where output is published, settled
        by 96d27f2. This change must not blur that distinction back together."""
        assert "--channel https://prefix.dev/blooop" in conda_publish_text()


class TestTheUpstreamURLHasOneAuthoritativeCopy:
    """`rust/Cargo.toml`'s `repository` key and the recipe's default are two
    hand-maintained copies of the same fact -- upstream's URL -- so per the
    house rule this test is the diff that keeps them from drifting apart."""

    def test_the_recipe_default_agrees_with_cargo_tomls_repository(self):
        cargo_repo_lines = [
            line for line in cargo_toml_text().splitlines() if line.strip().startswith("repository")
        ]
        assert len(cargo_repo_lines) == 1, cargo_repo_lines
        cargo_url = cargo_repo_lines[0].split("=", 1)[1].strip().strip('"')
        # Compared as-is, not with a `.git` bridged onto one side. They are the
        # same spelling now, and a comparison that reconciles a difference is a
        # comparison whose failure message prints two strings that look equal.
        assert UPSTREAM_URL == cargo_url, (
            f"recipe default {UPSTREAM_URL!r} and Cargo.toml repository "
            f"{cargo_url!r} have drifted apart"
        )


class TestTheDocumentedInstallGetsThisForksPackage:
    """The publish side being right does not make the consume side right.

    Upstream's channel carries `devlaunch` too, so an install line naming both
    channels can resolve to upstream's build. It resolves, it runs, and `pixi
    list` prints the version with the `+fork.1` segment dropped, so nothing
    about the result announces which package arrived. The build string is
    identical either way (`h8eb5a96_0`: the recipe hash does not depend on who
    builds it).

    **What protects against it is the version pin, not the channel order.**
    That correction is worth recording because the order was believed to be the
    protection for one commit: with both channels at 0.37.0, reordering them
    did change which package was resolved, and that single observation was
    generalised into "pixi uses strict channel priority, so the first channel
    wins and the version is never consulted". That is not pixi's default here.
    The reorder worked only because it broke a version *tie*. Once upstream's
    channel reached 0.40.0, the higher version won from the lower-priority
    channel and the install line silently fetched upstream again, order
    notwithstanding.

    Measured three ways: `channel-priority = "strict"` in a workspace manifest
    does hold this fork's channel, and so does an exact `==` pin, while the
    default with a plain range resolves
    `https://prefix.dev/blooop/linux-64/devlaunch-0.40.0-h8eb5a96_0.conda`.
    `pixi global install` has no `--channel-priority` flag, so a one-liner can
    only say this with a version, which is why the documented command carries
    one.

    The order is kept anyway: it costs nothing and it is what decides a genuine
    tie. Upstream's channel cannot be dropped, because the package's one run
    dependency is `devpod >=0.26.1` and devpod is not on conda-forge at all.
    """

    FORK_CHANNEL = "https://prefix.dev/jsmithrobotics/jsmithrobotics"
    UPSTREAM_CHANNEL = "https://prefix.dev/blooop"

    def _install_lines(self):
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        return [line for line in readme.splitlines() if "pixi global install" in line]

    def test_the_readme_documents_the_install_at_all(self):
        assert self._install_lines(), "README no longer shows how to install devlaunch"

    def test_this_forks_channel_comes_before_upstreams(self):
        for line in self._install_lines():
            assert self.FORK_CHANNEL in line, (
                f"install line does not name this fork's channel, so it installs "
                f"upstream's package: {line!r}"
            )
            assert self.UPSTREAM_CHANNEL in line, (
                f"install line drops upstream's channel, so `devpod >=0.26.1` "
                f"cannot resolve at all: {line!r}"
            )
            assert line.index(self.FORK_CHANNEL) < line.index(self.UPSTREAM_CHANNEL), (
                "upstream's channel precedes this fork's, and strict channel "
                f"priority means that installs upstream's build: {line!r}"
            )

    def test_the_forks_channel_is_the_namespaced_url(self):
        """`https://prefix.dev/jsmithrobotics` (once) is a 404 on
        `noarch/repodata.json`; the channel lives under owner/channel, spelled
        twice. Both spellings look plausible and only one resolves, which is
        why this is asserted rather than remembered."""
        for line in self._install_lines():
            assert "prefix.dev/jsmithrobotics/jsmithrobotics" in line, (
                f"the single-segment channel URL does not resolve: {line!r}"
            )

    def test_the_install_line_pins_the_version_that_ships(self):
        """The pin is what actually gets this fork's package, so it has to be
        this fork's version. A stale pin documents an install of something the
        tree no longer is; a missing one documents an install of upstream's
        build, which is the defect this class exists for."""
        cargo = cargo_toml_text()
        version = next(
            line.split("=", 1)[1].strip().strip('"')
            for line in cargo.splitlines()
            if line.strip().startswith("version")
        )
        for line in self._install_lines():
            assert f"devlaunch={version}" in line, (
                f"install line does not pin devlaunch={version}, so it can resolve "
                f"upstream's higher-versioned build instead: {line!r}"
            )
