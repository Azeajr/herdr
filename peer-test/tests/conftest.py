"""Fixtures for the herdr end-to-end scenarios.

`lab.py` is driven as a subprocess, never imported. Its JSON output and exit codes
(0 ok, 2 usage/precondition, 3 wait timed out, 4 assertion failed) are already the
contract, so a scenario here reads the same as the commands a human ran by hand — it
just gets assertions and teardown for free.

Two things every fixture in this file guarantees, because a leak is worse than a
failure: teardown runs on failure as well as success, and a failing test freezes an
evidence bundle *before* its lab is destroyed.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass
from pathlib import Path

import pytest

# `scripts/` for `_common`, and this directory for the sibling helpers, rather than
# relying on whichever paths pytest's import mode happens to have inserted by now.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import _boxes  # noqa: E402  (needs the path inserts above)
import _common  # noqa: E402  (needs the path inserts above)

REPO_ROOT = _common.REPO_ROOT
LAB_SCRIPT = REPO_ROOT / "peer-test" / "scripts" / "lab.py"
BOXES_SH = REPO_ROOT / "peer-test" / "docker" / "boxes.sh"

EXIT_OK = 0
EXIT_PRECONDITION = 2
EXIT_TIMEOUT = 3
EXIT_ASSERT = 4

#: The sidebar's peer group header, expanded and collapsed. Clicking the chevron is how
#: scenarios address the header: a socket peer's *label* is its socket path, which is
#: neither stable across labs nor short enough to survive truncation.
PEER_HEADER = "▾"
PEER_HEADER_COLLAPSED = "▸"


def harness_env() -> dict[str, str]:
    """The environment `lab.py` is spawned under.

    A suite run from inside a herdr pane would otherwise hand every lab server the
    outer session's socket overrides and startup cwd. `lab.py` scrubs them again for
    the herdr processes it spawns; scrubbing here as well keeps `cargo`, `docker` and
    `ssh` in the same clean environment.
    """
    env = os.environ.copy()
    for name in _common.INHERITED_HERDR_VARS:
        env.pop(name, None)
    return env


class LabCommandError(AssertionError):
    """A lab.py invocation that did not exit the way the scenario said it would."""

    def __init__(self, argv: list[str], result: LabResult, expected: tuple[int, ...]) -> None:
        wanted = ", ".join(str(code) for code in expected)
        detail = json.dumps(result.payload, indent=2)[:4000] if result.payload else result.stdout[:4000]
        super().__init__(
            f"lab.py {' '.join(argv)}\n"
            f"  exit {result.exit} (expected {wanted})\n"
            f"  stdout: {detail}\n"
            f"  stderr: {result.stderr.strip()[:2000]}"
        )
        self.result = result


@dataclass
class LabResult:
    """One lab.py invocation: its exit code and whatever JSON it printed."""

    exit: int
    payload: dict
    stdout: str
    stderr: str

    def __getitem__(self, key: str):
        return self.payload[key]

    def get(self, key: str, default=None):
        return self.payload.get(key, default)


class Lab:
    """A live lab, addressed the way the README addresses it."""

    def __init__(self, name: str, binary: Path) -> None:
        self.name = name
        self.binary = binary
        self.root = Path("/tmp") / f"hl-{name}"

    # --- plumbing ---------------------------------------------------------

    def run(
        self,
        *args: str,
        expect: int | tuple[int, ...] = EXIT_OK,
        timeout: float = 180.0,
    ) -> LabResult:
        """Run one lab.py subcommand and return its parsed answer."""
        argv = [str(arg) for arg in args]
        proc = subprocess.run(
            ["uv", "run", "--script", str(LAB_SCRIPT), "--lab", self.name, "--json", *argv],
            capture_output=True,
            text=True,
            cwd=REPO_ROOT,
            env=harness_env(),
            timeout=timeout,
        )
        try:
            payload = json.loads(proc.stdout)
        except json.JSONDecodeError:
            payload = {}
        result = LabResult(proc.returncode, payload, proc.stdout, proc.stderr)
        wanted = (expect,) if isinstance(expect, int) else tuple(expect)
        if result.exit not in wanted:
            raise LabCommandError(argv, result, wanted)
        return result

    # --- lifecycle --------------------------------------------------------

    def up(self, instances: str = "a,b", peers: tuple[str, ...] = (), **env: str) -> LabResult:
        args = ["up", "--instances", instances, "--bin", str(self.binary)]
        for spec in peers:
            args += ["--peer", spec]
        for key, value in env.items():
            args += ["--env", f"{key}={value}"]
        return self.run(*args, timeout=300.0)

    def down(self) -> LabResult:
        """Stop servers and clients but keep the lab root, so config survives."""
        return self.run("down")

    def restart(self, instances: str, peers: tuple[str, ...] = ()) -> LabResult:
        """A real cold start: every server exits, then boots again off its own config."""
        self.down()
        return self.up(instances=instances, peers=peers)

    def destroy(self) -> LabResult:
        return self.run("destroy", expect=(EXIT_OK, EXIT_PRECONDITION))

    def evidence(self, name: str, note: str) -> LabResult:
        return self.run("evidence", name, "--note", note, expect=(EXIT_OK, EXIT_PRECONDITION))

    # --- servers ----------------------------------------------------------

    def status(self) -> dict:
        return self.run("status").payload

    def state(self, instance: str) -> dict:
        return self.run("state", instance).payload

    def cli(self, instance: str, *args: str, expect: int | tuple[int, ...] = EXIT_OK) -> LabResult:
        return self.run("cli", instance, "--", *args, expect=expect)

    def api(self, instance: str, method: str, params: dict | None = None) -> dict:
        args = ["api", instance, method]
        if params is not None:
            args += ["--params", json.dumps(params)]
        return self.run(*args)["response"]

    def instance_id(self, instance: str) -> str:
        return self.state(instance)["instance_id"]

    def peers(self, instance: str) -> list[dict]:
        return self.cli(instance, "peer", "list", "--json")["result"]["peers"]

    def wait_peer_enumerated(self, instance: str, peer: str, timeout: float = 30.0) -> dict:
        """Block until `instance` has a peer's workspace list.

        Asserting that something is *absent* from a screen is only worth anything once
        the thing that would have drawn it has arrived. Enumeration is that gate: after a
        restart the header is missing for a second or two no matter what the config says.
        """
        deadline = time.monotonic() + timeout
        entry = None
        while time.monotonic() < deadline:
            entry = next((item for item in self.state(instance)["peers"] if item["name"] == peer), None)
            if entry and entry.get("workspaces"):
                return entry
            time.sleep(0.5)
        raise AssertionError(f"{instance} never enumerated peer {peer!r} within {timeout}s: {entry}")

    def config_text(self, instance: str) -> str:
        """The instance's own config.toml, or "" before herdr has written one."""
        status = self.status()
        sock = Path(status["instances"][instance]["sock"])
        path = sock.parent / "config.toml"
        return path.read_text() if path.is_file() else ""

    # --- clients ----------------------------------------------------------

    def ui_open(self, instance: str, client: str, cols: int = 120, rows: int = 40) -> LabResult:
        """Open a client and clear onboarding, which otherwise eats every chrome click."""
        opened = self.run("ui", "open", instance, "--client", client, "--cols", cols, "--rows", rows)
        if opened.get("gate") == "onboarding":
            self.run("ui", "onboard", client)
        return opened

    def ui_close(self, client: str) -> LabResult:
        return self.run("ui", "close", client, expect=(EXIT_OK, EXIT_PRECONDITION))

    def screen(self, client: str) -> list[str]:
        return self.run("ui", "screen", client)["lines"]

    def find(self, client: str, text: str) -> list[dict]:
        return self.run("ui", "find", client, text, expect=(EXIT_OK, EXIT_PRECONDITION))["matches"]

    def click(
        self,
        client: str,
        *,
        text: str | None = None,
        col: int | None = None,
        row: int | None = None,
        pane: str | None = None,
        control: str | None = None,
        index: int = 0,
        button: str = "left",
        settle: float | None = None,
        expect: int | tuple[int, ...] = EXIT_OK,
    ) -> LabResult:
        args = ["ui", "click", client, "--button", button]
        if settle is not None:
            args += ["--settle", settle]
        if control is not None:
            args += ["--control", control]
        if text is not None:
            args += ["--text", text, "--index", index]
        if col is not None:
            args += ["--col", col]
        if row is not None:
            args += ["--row", row]
        if pane is not None:
            args += ["--pane", pane]
        # A hand-counted coordinate that lands on blank space closes an open menu and
        # dispatches nothing, which reads exactly like a dead button. Never let that
        # pass silently in a test.
        if col is not None or row is not None:
            args.append("--require-hit")
        return self.run(*args, expect=expect)

    def hitbox(self, client: str) -> dict:
        """Where the server that drew this client thinks its controls are."""
        return self.run("ui", "hitbox", client).payload

    def control(
        self,
        client: str,
        name: str,
        *,
        timeout: float = 5.0,
        expect: int | tuple[int, ...] = EXIT_OK,
    ) -> LabResult:
        return self.run("ui", "hitbox", client, "--control", name, "--timeout", timeout, expect=expect)

    def keys(self, client: str, spec: str) -> LabResult:
        return self.run("ui", "keys", client, spec)

    def type_text(self, client: str, text: str) -> LabResult:
        return self.run("ui", "text", client, text)

    def wait_for(self, client: str, needle: str, timeout: float = 15.0) -> LabResult:
        return self.run("ui", "wait", client, "--contains", needle, "--timeout", timeout)

    def wait_gone(self, client: str, needle: str, timeout: float = 15.0) -> LabResult:
        return self.run("ui", "wait", client, "--contains", needle, "--gone", "--timeout", timeout)

    def sees(self, client: str, needle: str) -> bool:
        return bool(self.find(client, needle))

    # --- peer sidebar -----------------------------------------------------

    def peer_header(self, client: str, chevron: str = PEER_HEADER) -> str:
        """The peer group header line, or "" when no group is on screen."""
        matches = self.find(client, chevron)
        return matches[0]["line"] if matches else ""

    def open_peer_menu(self, client: str, chevron: str = PEER_HEADER) -> LabResult:
        """Right-click the peer header, which is where its context menu comes from."""
        return self.click(client, text=chevron, button="right")

    def open_peer_workspace(self, client: str, label: str) -> LabResult:
        """Left-click the peer header, then pick a workspace out of the picker."""
        self.click(client, text=PEER_HEADER)
        self.wait_for(client, "open workspace on")
        return self.click(client, text=label)

    def logs(self, which: str = "all", pattern: str | None = None, tail: int = 50) -> list[dict]:
        args = ["logs", which, "--tail", tail]
        if pattern:
            args += ["--grep", pattern]
        return self.run(*args)["entries"]


# ---------------------------------------------------------------------------
# fixtures
# ---------------------------------------------------------------------------


@pytest.hookimpl(wrapper=True, tryfirst=True)
def pytest_runtest_makereport(item, call):
    """Record each phase's result so a fixture can tell whether its test failed."""
    report = yield
    setattr(item, f"rep_{report.when}", report)
    return report


def pytest_collection_modifyitems(config, items):
    """Mark by location: boxes/ needs Docker, stress/ is deliberately heavy, rest is e2e."""
    for item in items:
        path = str(item.path)
        if "/boxes/" in path:
            item.add_marker(pytest.mark.boxes)
        elif "/stress/" in path:
            item.add_marker(pytest.mark.stress)
        else:
            item.add_marker(pytest.mark.e2e)


@pytest.fixture(scope="session")
def herdr_bin() -> Path:
    """One debug build for the whole suite.

    Labs get it through `up --bin`; the Docker boxes bind-mount the same file, so both
    halves of the suite always test the binary this checkout just produced.
    """
    zig = _common.resolve_zig()
    if warning := _common.zig_version_warning(zig):
        print(f"\n{warning}", file=sys.stderr)
    return _common.cargo_build(zig=zig, quiet=True)


@pytest.fixture
def lab_factory(request, herdr_bin):
    """Make labs that are always torn down, and that leave evidence when they fail.

    Lab roots have to stay short — a server binds `<config>/<app_dir>/herdr.sock` and a
    deep root overflows `sun_path` — so names are five hex characters, not the node id.
    """
    if shutil.which("tmux") is None:
        pytest.skip("tmux is not installed; lab clients live in tmux sessions")
    made: list[Lab] = []

    def factory(instances: str = "a,b", peers: tuple[str, ...] = (), **env: str) -> Lab:
        lab = Lab(f"t{uuid.uuid4().hex[:5]}", herdr_bin)
        made.append(lab)
        lab.up(instances=instances, peers=peers, **env)
        return lab

    yield factory

    failed = getattr(request.node, "rep_call", None) is None or request.node.rep_call.failed
    leaked = []
    for lab in made:
        if failed:
            slug = re.sub(r"[^a-z0-9]+", "-", request.node.name.lower()).strip("-")[:40]
            lab.evidence(slug or "failure", f"failed: {request.node.nodeid}")
        if not lab.destroy().get("root_removed"):
            leaked.append(str(lab.root))
    # A leaked lab root is a defect in teardown, and the next run inherits it. Say so
    # instead of letting them pile up in /tmp unnoticed.
    assert not leaked, f"lab roots survived destroy: {leaked}"


@pytest.fixture
def peer_lab(lab_factory) -> Lab:
    """The common topology: two servers, `a` peered to `b` over a socket."""
    return lab_factory(instances="a,b", peers=("a->b",))


@pytest.fixture
def peer_ui(peer_lab) -> Lab:
    """`peer_lab`, plus one workspace on `b` and a client on `a` that has enumerated it.

    Stopping here rather than opening the workspace is deliberate: whether opening it is
    a click, a pick, or a CLI call is exactly what several of these scenarios are for.
    """
    peer_lab.cli("b", "workspace", "create", "--label", "remote-ws")
    peer_lab.ui_open("a", "A")
    # The header carries `<opened>/<enumerated>`, so "0/1" is the peer's workspace list
    # having arrived — clicking before that races the enumeration.
    peer_lab.wait_for("A", "0/1")
    return peer_lab


@pytest.fixture(scope="session")
def boxes(herdr_bin):
    """The three Docker peer boxes, up for the session and down afterwards.

    They bind-mount `herdr_bin`, so depending on it is not decoration: without it the
    boxes could run a stale binary from an earlier build, or none at all.
    """
    if shutil.which("docker") is None:
        pytest.skip("docker is not installed")
    up = subprocess.run(
        [str(BOXES_SH), "up"], capture_output=True, text=True, cwd=REPO_ROOT, env=harness_env(), timeout=900
    )
    if up.returncode != 0:
        pytest.skip(f"boxes.sh up failed: {up.stderr.strip()[-500:]}")
    config = REPO_ROOT / "peer-test" / "docker" / ".secrets" / "ssh_config"
    try:
        yield BoxSet(config)
    finally:
        subprocess.run(
            [str(BOXES_SH), "down"], capture_output=True, text=True, cwd=REPO_ROOT, env=harness_env(), timeout=300
        )


class Netem:
    """Degrade one box's traffic toward another, through `boxes.sh netem`.

    Scoped by destination on purpose: only packets addressed to the other box are
    shaped, so the ssh these scenarios observe through keeps its normal path even at
    100% loss. Unscoped shaping would take the observer down with the subject.
    """

    def __init__(self) -> None:
        self._shaped: set[str] = set()

    def _run(self, *args: str) -> str:
        proc = subprocess.run(
            [str(BOXES_SH), "netem", *args],
            capture_output=True,
            text=True,
            cwd=REPO_ROOT,
            env=harness_env(),
            timeout=60,
        )
        assert proc.returncode == 0, (
            f"boxes.sh netem {' '.join(args)}\n"
            f"  exit {proc.returncode}\n"
            f"  stdout: {proc.stdout.strip()[:1000]}\n"
            f"  stderr: {proc.stderr.strip()[:1000]}"
        )
        return proc.stdout

    def apply(self, box: str, delay: str, loss: str, *, to: str) -> str:
        self._shaped.add(box)
        return self._run(box, "--to", to, delay, loss)

    def partition(self, box: str, *, to: str) -> str:
        """Drop everything `box` sends to `to`.

        One direction is enough: the other box's packets still arrive, nothing answers
        them, and every connection between the two stalls.
        """
        return self.apply(box, "0ms", "100%", to=to)

    def clear(self, box: str) -> str:
        self._shaped.discard(box)
        return self._run(box, "clear")

    def clear_all(self) -> None:
        for box in sorted(self._shaped):
            self._run(box, "clear")
        self._shaped.clear()


@pytest.fixture
def netem(boxes):
    """`tc netem` on the boxes, always cleared afterwards.

    The containers are session-scoped, so a rule left behind does not fail this test —
    it silently degrades every scenario that runs after it.
    """
    shaper = Netem()
    try:
        yield shaper
    finally:
        shaper.clear_all()


@pytest.fixture
def box_servers(boxes):
    """A clean headless herdr on box1 and box2. box3 stays deliberately empty."""
    for box in _boxes.PAIR:
        _boxes.stop_and_wipe(boxes, box)
        _boxes.start_server(boxes, box)
    for box in _boxes.PAIR:
        _boxes.wait_ready(boxes, box)
    yield boxes
    for box in _boxes.PAIR:
        _boxes.stop_and_wipe(boxes, box)


class BoxSet:
    """`ssh box1 '<cmd>'` against the throwaway containers, with their own ssh config."""

    def __init__(self, ssh_config: Path) -> None:
        self.ssh_config = ssh_config

    def ssh(
        self,
        box: str,
        command: str,
        *,
        expect: int | tuple[int, ...] = 0,
        timeout: float = 120.0,
    ) -> subprocess.CompletedProcess:
        proc = subprocess.run(
            ["ssh", "-F", str(self.ssh_config), "-o", "BatchMode=yes", box, command],
            capture_output=True,
            text=True,
            env=harness_env(),
            timeout=timeout,
        )
        wanted = (expect,) if isinstance(expect, int) else tuple(expect)
        assert proc.returncode in wanted, (
            f"ssh {box} {command!r}\n"
            f"  exit {proc.returncode} (expected {wanted})\n"
            f"  stdout: {proc.stdout.strip()[:2000]}\n"
            f"  stderr: {proc.stderr.strip()[:2000]}"
        )
        return proc

    def herdr_json(self, box: str, command: str, *, expect: int | tuple[int, ...] = 0) -> dict:
        """Run a herdr CLI command on a box and parse the JSON it prints."""
        proc = self.ssh(box, command, expect=expect)
        return json.loads(proc.stdout)
