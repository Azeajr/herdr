"""Build and environment helpers shared by lab.py and its pytest fixtures.

lab.py is a `uv run` single-file script, so its directory is `sys.path[0]` and this
sibling imports without packaging. This module stays stdlib-only because the fixtures
import it directly too.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
REQUIRED_ZIG = "0.15.2"
EVIDENCE_DIR = Path(__file__).resolve().parents[1] / "evidence"

#: Every herdr in a lab must run without these. A herdr started from inside a herdr pane
#: inherits socket overrides and would talk to the wrong server; `HERDR_STARTUP_CWD` seeds
#: a startup workspace nobody asked for; and a TUI launched from inside a pane refuses to
#: start at all ("nested herdr is disabled"). The test harness scrubs the same list before
#: it spawns `lab.py`, so a suite run from inside a herdr session behaves like one outside.
INHERITED_HERDR_VARS = (
    "HERDR_SOCKET_PATH",
    "HERDR_CLIENT_SOCKET_PATH",
    "HERDR_STARTUP_CWD",
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
)


class LabError(RuntimeError):
    """A precondition the caller stated wrongly, or a step that could not run.

    Carries the process exit code the caller should use.
    """

    def __init__(self, message: str, *, code: int = 2, **details: object) -> None:
        super().__init__(message)
        self.code = code
        self.details = details


def resolve_zig() -> str:
    """Pick the zig binary to build with, respecting a caller-set $ZIG."""
    if zig := os.environ.get("ZIG"):
        return zig
    if (candidate := Path("/opt/zig0.15/zig")).is_file() and os.access(candidate, os.X_OK):
        return str(candidate)
    if found := shutil.which("zig"):
        return found
    raise LabError(
        f"no zig found. Herdr needs zig {REQUIRED_ZIG}; set ZIG=/path/to/zig and rerun."
    )


def zig_version_warning(zig: str) -> str | None:
    """A warning string when $ZIG is not the version herdr builds against, else None."""
    result = subprocess.run([zig, "version"], capture_output=True, text=True)
    version = result.stdout.strip() or "unknown"
    if version.startswith(REQUIRED_ZIG):
        return None
    return f"$ZIG ({zig}) reports version '{version}', not {REQUIRED_ZIG} — build may fail."


def cargo_build(
    *,
    zig: str | None = None,
    quiet: bool = False,
) -> Path:
    """Build the checkout's debug herdr and return its path. Raises on failure."""
    zig = zig or resolve_zig()
    env = os.environ.copy()
    env["ZIG"] = zig
    cmd = ["cargo", "build", "--locked"]
    result = subprocess.run(
        cmd,
        cwd=REPO_ROOT,
        env=env,
        stdout=subprocess.DEVNULL if quiet else None,
        stderr=subprocess.PIPE if quiet else None,
        text=True,
    )
    if result.returncode != 0:
        tail = (result.stderr or "").strip().splitlines()[-20:]
        raise LabError(
            "cargo build failed",
            code=2,
            zig=zig,
            stderr="\n".join(tail),
        )
    return REPO_ROOT / "target" / "debug" / "herdr"


def git_ref() -> tuple[str, str]:
    """Current branch and short commit of the checkout the scripts live in."""
    try:
        branch = subprocess.check_output(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"], cwd=REPO_ROOT, text=True
        ).strip()
        commit = subprocess.check_output(
            ["git", "rev-parse", "--short", "HEAD"], cwd=REPO_ROOT, text=True
        ).strip()
    except (subprocess.CalledProcessError, OSError):
        return ("unknown", "unknown")
    return branch, commit


def scrubbed_env(config: Path, state: Path, **extra: str) -> dict[str, str]:
    """The environment every lab herdr — server, client or CLI — must run under.

    A herdr started from inside a herdr pane inherits socket overrides and would talk to
    the wrong server; `HERDR_STARTUP_CWD` seeds a startup workspace nobody asked for; and
    a TUI launched from inside a pane refuses to start at all ("nested herdr is
    disabled"). Strip all of it, then point XDG at the caller's isolated pair.
    """
    env = os.environ.copy()
    for name in INHERITED_HERDR_VARS:
        env.pop(name, None)
    env["XDG_CONFIG_HOME"] = str(config)
    env["XDG_STATE_HOME"] = str(state)
    env.update(extra)
    return env
