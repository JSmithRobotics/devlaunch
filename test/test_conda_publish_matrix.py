"""`conda-publish.yml` builds and ships both linux-64 and linux-aarch64, and the
guards here are for the two ways that can quietly regress to linux-64 only.

`.github/workflows/devcontainer-prebuild.yml` set the precedent this workflow
copies: a `fail-fast: false` matrix over `ubuntu-latest` (amd64) and
`ubuntu-24.04-arm` (arm64), both native GitHub-hosted runners, no
cross-compilation and no QEMU.

The two failure modes worth naming, because both leave the workflow green:

1. The `build` job stops being a matrix (or the matrix drops a runner), so
   only one architecture's package is ever produced.
2. The `build` job keeps the matrix but `publish` downloads only one
   artifact -- either because the two legs upload under a name that collides
   (the second overwrites the first) or because `publish`'s download step
   names one fixed artifact instead of pulling everything the matrix made.
   This is the half that is easy to miss: both jobs report success, and the
   channel is still missing an architecture.

There is no YAML/jinja parser among this project's dependencies (same
constraint `test_conda_source_repo.py` documents), so these are text/structure
assertions against the job bodies, sliced out by their headers -- not
substring checks against the whole file, which is what let a comment
mentioning the right words pass a guard for the wrong reason elsewhere in this
project's history.
"""

from pathlib import Path

ROOT = Path(__file__).parent.parent
CONDA_PUBLISH = ROOT / ".github" / "workflows" / "conda-publish.yml"

AMD64_RUNNER = "ubuntu-latest"
ARM64_RUNNER = "ubuntu-24.04-arm"
BUILD_DEPENDENCY_CHANNEL = "https://prefix.dev/blooop"


def workflow_text() -> str:
    assert CONDA_PUBLISH.is_file(), f"{CONDA_PUBLISH} is the workflow this module is about"
    return CONDA_PUBLISH.read_text(encoding="utf-8")


def job_body(text: str, job_key: str) -> str:
    """The text of one top-level job, from its `  <job_key>:` header up to the
    next job header at the same (2-space) indent -- or end of file for the
    last job.

    Slicing by job is what keeps a check on `build` from being satisfied by
    something that only appears in `check` or `publish` (all three jobs share
    `runs-on: ubuntu-latest` today, for instance).
    """
    lines = text.splitlines()
    job_header = f"  {job_key}:"
    start = next(i for i, line in enumerate(lines) if line == job_header)
    end = len(lines)
    for i in range(start + 1, len(lines)):
        line = lines[i]
        # A top-level job header: exactly two-space indent, a bare identifier,
        # then a colon. Anything more indented belongs to the job above it.
        if (
            line
            and not line.startswith("   ")
            and line.startswith("  ")
            and line.rstrip().endswith(":")
        ):
            end = i
            break
    return "\n".join(lines[start:end])


class TestTheBuildJobIsAMatrixOverBothArchitectures:
    """Failure mode 1: the build job stops covering arm64 at all."""

    def test_the_build_job_declares_a_strategy_matrix(self):
        build = job_body(workflow_text(), "build")
        assert "strategy:" in build, "build job has no strategy/matrix at all -- single arch only"
        assert "matrix:" in build

    def test_the_matrix_includes_an_amd64_and_an_arm64_runner(self):
        """Structural: both runner names have to appear as `runner:` values
        inside the build job's matrix, not merely anywhere in the file (the
        `check` and `publish` jobs both legitimately say `ubuntu-latest` for
        their own `runs-on`, which must not satisfy this)."""
        build = job_body(workflow_text(), "build")
        matrix_idx = build.index("matrix:")
        matrix_section = build[matrix_idx:]
        runner_lines = [line.strip() for line in matrix_section.splitlines() if "runner:" in line]
        runners = {line.split(":", 1)[1].strip() for line in runner_lines}
        assert AMD64_RUNNER in runners, f"no amd64 runner in the build matrix: {runners!r}"
        assert ARM64_RUNNER in runners, f"no arm64 runner in the build matrix: {runners!r}"

    def test_runs_on_reads_the_matrix_rather_than_a_fixed_runner(self):
        """`runs-on` has to key off `matrix.runner`, or the matrix above is
        declared but never actually used to pick where each leg runs."""
        build = job_body(workflow_text(), "build")
        runs_on_line = next(
            line.strip() for line in build.splitlines() if line.strip().startswith("runs-on:")
        )
        assert "matrix.runner" in runs_on_line, (
            f"build job does not run on the matrix runner: {runs_on_line!r}"
        )

    def test_the_matrix_does_not_abandon_a_failed_leg(self):
        """`fail-fast: false`, same as devcontainer-prebuild.yml: one
        architecture's failure must not cancel the other leg's package."""
        build = job_body(workflow_text(), "build")
        assert "fail-fast: false" in build


class TestThePublishJobShipsEveryArtifactTheMatrixProduces:
    """Failure mode 2: the matrix is intact but the channel still only gets
    one architecture, because the artifacts collide on name or because publish
    only ever asked for one."""

    def test_each_matrix_leg_uploads_its_package_under_a_distinct_name(self):
        """If both legs upload under the same artifact name, the second
        upload overwrites the first (actions/upload-artifact does not merge),
        so publish would still only ever see one architecture's package no
        matter how it downloads. The name has to vary per leg."""
        build = job_body(workflow_text(), "build")
        upload_idx = build.index("upload-artifact")
        # The `name:` key belongs to the `with:` block right after the
        # `uses: actions/upload-artifact...` line.
        after_upload = build[upload_idx:]
        name_line = next(
            line.strip() for line in after_upload.splitlines() if line.strip().startswith("name:")
        )
        assert "matrix.arch" in name_line or "matrix.runner" in name_line, (
            f"upload-artifact name does not vary per matrix leg, so one architecture's "
            f"package silently overwrites the other's: {name_line!r}"
        )

    def test_publish_downloads_a_pattern_of_artifacts_not_one_fixed_name(self):
        """`publish` has to fetch every artifact the matrix produced. A
        `download-artifact` step with a single literal `name:` is exactly the
        defect this whole guard exists for: it built two packages and shipped
        one, both jobs green."""
        publish = job_body(workflow_text(), "publish")
        download_idx = publish.index("download-artifact")
        with_block = publish[download_idx:]
        # There must be no fixed, non-wildcarded `name:` selecting a single
        # artifact in this step.
        name_lines = [
            line.strip() for line in with_block.splitlines() if line.strip().startswith("name:")
        ]
        assert not name_lines, (
            f"publish's download-artifact step names one fixed artifact instead of "
            f"pulling every architecture's: {name_lines!r}"
        )
        pattern_lines = [
            line.strip() for line in with_block.splitlines() if line.strip().startswith("pattern:")
        ]
        assert pattern_lines, (
            "publish's download-artifact step has no pattern, so it fetches nothing set-wise"
        )
        assert "*" in pattern_lines[0], (
            f"download pattern is not a wildcard, so it still names one artifact: {pattern_lines[0]!r}"
        )


class TestTheBuildDependencyChannelStaysBlooops:
    """`--channel https://prefix.dev/blooop` resolves build DEPENDENCIES (the
    compilers) from where they are maintained -- unrelated to
    DEVLAUNCH_SOURCE_REPO or where output is published (settled by 96d27f2,
    reasserted by test_conda_source_repo.py's
    TestCondaPublishActuallySetsTheVariable.test_the_build_dependency_channel_is_left_alone).
    This class re-asserts it here so a future edit chasing "publish to our own
    channel" cannot quietly repoint the compiler channel at a fork's channel,
    which does not carry compilers at all."""

    def test_the_rattler_build_action_still_resolves_dependencies_from_blooop(self):
        build = job_body(workflow_text(), "build")
        build_args_line = next(
            line.strip() for line in build.splitlines() if line.strip().startswith("build-args:")
        )
        assert f"--channel {BUILD_DEPENDENCY_CHANNEL}" in build_args_line, (
            f"build-args no longer resolves compilers from {BUILD_DEPENDENCY_CHANNEL}: "
            f"{build_args_line!r}"
        )

    def test_the_channel_is_a_literal_and_not_this_forks_publish_variable(self):
        """The publish destination (`vars.CONDA_CHANNEL`) and the build
        dependency channel are different things on purpose. If the
        build-args line ever starts reading `vars.CONDA_CHANNEL` instead of
        the literal, a fork publishing to its own channel would also try to
        resolve compilers from it and fail, since that channel carries no
        compilers."""
        build = job_body(workflow_text(), "build")
        build_args_line = next(
            line.strip() for line in build.splitlines() if line.strip().startswith("build-args:")
        )
        assert "vars.CONDA_CHANNEL" not in build_args_line, (
            f"build-args now resolves the compiler channel from the publish variable: {build_args_line!r}"
        )
