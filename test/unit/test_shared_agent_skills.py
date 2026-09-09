"""Shared skill discovery and write protection across the feature's mounts."""

import json
import shlex
import subprocess
from pathlib import Path

import pytest

from unit.test_claude_code_feature_mounts import FEATURE_JSON
from unit.test_devcontainer_manifest import (
    DEVCONTAINER_JSON,
    parse_mount,
    run_initialize_command,
    strip_jsonc_comments,
)
from unit.test_init_host_heals_stale_mounts import NAMESPACE


def skill_mounts():
    return [parse_mount(spec) for spec in json.loads(FEATURE_JSON.read_text())["mounts"]]


def test_codex_discovery_and_shared_bodies_are_mounted_read_only():
    mounts = {mount["source"]: mount for mount in skill_mounts()}
    for relative in (".agents/skills", ".claude/shared-skills"):
        mount = mounts.get(f"${{localEnv:HOME}}/{relative}")
        assert mount is not None, f"the feature does not expose {relative}"
        assert mount["type"] == "bind"
        assert "readonly" in mount, f"the container can rewrite the host's {relative}"
        config = json.loads(FEATURE_JSON.read_text())["containerEnv"]["CLAUDE_CONFIG_DIR"]
        assert mount["target"] == f"{config.removesuffix('/.claude')}/{relative}"


def test_initialize_creates_missing_skill_roots_without_changing_existing_skills(tmp_path):
    home = tmp_path / "host user"
    home.mkdir()
    shared = home / ".claude/shared-skills/example"
    shared.mkdir(parents=True)
    body = shared / "SKILL.md"
    body.write_text("the existing instructions\n")
    discovery = home / ".agents/skills"
    discovery.mkdir(parents=True)
    link = discovery / "example"
    link.symlink_to("../../.claude/shared-skills/example")
    config = json.loads(strip_jsonc_comments(DEVCONTAINER_JSON.read_text()))
    before = (body.stat().st_mtime_ns, link.lstat().st_mtime_ns)

    for _ in range(2):
        result = run_initialize_command(config, home)
        assert result.returncode == 0, result.stderr
        for mount in skill_mounts():
            source = mount["source"].replace("${localEnv:HOME}", str(home))
            assert Path(source).is_dir()

    assert body.read_text() == "the existing instructions\n"
    assert link.readlink().as_posix() == "../../.claude/shared-skills/example"
    assert before == (body.stat().st_mtime_ns, link.lstat().st_mtime_ns)


@pytest.mark.skipif(not NAMESPACE, reason="requires a private mount namespace")
def test_both_shared_layouts_resolve_and_stay_read_only_in_a_different_home(tmp_path):
    """Exercise the actual mounts and writes, including through symlink targets."""
    host = tmp_path / "host user"
    guest = tmp_path / "container user"
    host.mkdir()
    config = json.loads(strip_jsonc_comments(DEVCONTAINER_JSON.read_text()))
    result = run_initialize_command(config, host)
    assert result.returncode == 0, result.stderr

    # One skill stored in the standard shared root, another in the Claude
    # config mount. Both agents see both skills through relative links.
    for relative in (".agents/skills/native", ".claude/shared-skills/shared"):
        folder = host / relative
        folder.mkdir(parents=True, exist_ok=True)
        (folder / "SKILL.md").write_text("original\n")
    (host / ".claude/skills/native").symlink_to("../../.agents/skills/native")
    (host / ".claude/skills/shared").symlink_to("../shared-skills/shared")
    (host / ".agents/skills/shared").symlink_to("../../.claude/shared-skills/shared")
    (host / ".claude/.credentials.json").write_text("original\n")

    feature_home = json.loads(FEATURE_JSON.read_text())["containerEnv"][
        "CLAUDE_CONFIG_DIR"
    ].removesuffix("/.claude")
    script = []
    for mount in skill_mounts():
        source = mount["source"].replace("${localEnv:HOME}", str(host))
        relative = mount["target"].removeprefix(feature_home + "/")
        target = guest / relative
        target.mkdir(parents=True, exist_ok=True)
        script.append(f"mount --bind {shlex.quote(source)} {shlex.quote(str(target))}")
        if "readonly" in mount:
            script.append(f"mount -o remount,bind,ro {shlex.quote(str(target))}")

    for agent in (".agents", ".claude"):
        for name in ("native", "shared"):
            body = shlex.quote(str(guest / agent / "skills" / name / "SKILL.md"))
            script.append(f'test "$(cat {body})" = original')
            script.append(f"if echo injected >> {body}; then exit 1; fi")
    # Replacing a body on the host must reach both agents while the directory
    # mount keeps writes from the container refused.
    for relative in (".agents/skills/native", ".claude/shared-skills/shared"):
        folder = shlex.quote(str(host / relative))
        script.append(f"echo updated > {folder}/replacement")
        script.append(f"mv {folder}/replacement {folder}/SKILL.md")
    for agent in (".agents", ".claude"):
        for name in ("native", "shared"):
            body = shlex.quote(str(guest / agent / "skills" / name / "SKILL.md"))
            script.append(f'test "$(cat {body})" = updated')
            script.append(f"if echo injected >> {body}; then exit 1; fi")
    credentials = shlex.quote(str(guest / ".claude/.credentials.json"))
    script.append(f"echo refreshed > {credentials}")
    result = subprocess.run(
        [*NAMESPACE, "sh", "-eu", "-c", "\n".join(script)],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    assert (host / ".claude/.credentials.json").read_text() == "refreshed\n"
    for relative in (".agents/skills/native", ".claude/shared-skills/shared"):
        assert (host / relative / "SKILL.md").read_text() == "updated\n"
