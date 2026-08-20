"""Helpers for the Docker peer boxes.

Each box is a genuinely separate machine as far as herdr can tell — its own rootfs,
hostname, `$HOME`, `PATH` and network namespace — which is the whole reason these
scenarios exist: `resolve_peer_remote_herdr` has to find a binary on a filesystem this
host does not share.
"""

from __future__ import annotations

import time

PAIR = ("box1", "box2")

#: Every directory a herdr on a box writes to. Wiped between tests so a peer registry, a
#: session snapshot or a `[peer_hidden]` entry never leaks from one scenario into the
#: next — the containers are session-scoped and outlive both.
BOX_STATE = "~/.config/herdr* ~/.local/state/herdr* ~/.local/share/herdr*"

#: Where a box's herdr writes its server log, under whichever of those it picked.
LOG_GLOB = "~/.config/herdr*/herdr-server.log"


def stop_and_wipe(boxes, box: str) -> None:
    boxes.ssh(box, f"herdr server stop >/dev/null 2>&1; rm -rf {BOX_STATE}; true")


def start_server(boxes, box: str, *, log: str = "") -> None:
    # `herdr server` with no subcommand *is* the headless server; `herdr server start`
    # is not a thing and its error points at the TUI instead.
    env = f"HERDR_LOG={log} " if log else ""
    boxes.ssh(box, f"{env}setsid herdr server </dev/null >/tmp/herdr-server.log 2>&1 & true")


def server_log(boxes, box: str) -> str:
    """The box's own herdr server log.

    A debug build writes under `herdr-dev`, so the directory is globbed rather than
    named — the same glob `BOX_STATE` wipes.
    """
    return boxes.ssh(box, f"cat {LOG_GLOB} 2>/dev/null || true").stdout


def wait_ready(boxes, box: str, timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if boxes.ssh(box, "herdr workspace list >/dev/null 2>&1; echo $?").stdout.strip() == "0":
            return
        time.sleep(0.5)
    log = boxes.ssh(box, "tail -20 /tmp/herdr-server.log || true").stdout
    raise AssertionError(f"herdr server on {box} never answered within {timeout}s:\n{log}")


def wait_peer_connected(boxes, box: str, peer: str, timeout: float = 90.0) -> dict:
    """Poll until `peer` is connected *and* has reported its workspaces."""
    deadline = time.monotonic() + timeout
    entry = None
    while time.monotonic() < deadline:
        listing = boxes.herdr_json(box, "herdr peer list --json")
        entry = next((item for item in listing["result"]["peers"] if item["name"] == peer), None)
        if entry and entry["connection"] == "connected" and entry["workspaces"]:
            return entry
        time.sleep(1.0)
    raise AssertionError(f"{box} never saw peer {peer!r} connect and enumerate in {timeout}s: {entry}")


def wait_peer_connection(boxes, box: str, peer: str, kind: str, timeout: float = 60.0) -> dict:
    """Poll until `peer`'s control connection reaches `kind`.

    The counterpart to `wait_peer_connected` for the states that are not healthy:
    `reconnecting` is what a partition looks like from the other side.
    """
    deadline = time.monotonic() + timeout
    entry = None
    while time.monotonic() < deadline:
        listing = boxes.herdr_json(box, "herdr peer list --json")
        entry = next((item for item in listing["result"]["peers"] if item["name"] == peer), None)
        if entry and entry["connection"] == kind:
            return entry
        time.sleep(1.0)
    raise AssertionError(f"{box} never saw peer {peer!r} reach {kind!r} in {timeout}s: {entry}")


def panes(boxes, box: str) -> list[dict]:
    return boxes.herdr_json(box, "herdr pane list")["result"]["panes"]


def wait_pane_title(boxes, box: str, pane_id: str, timeout: float = 30.0) -> str:
    """Poll until a pane reports a terminal title.

    The field is absent until the shell on the far side emits one, so reading it
    straight after a command tests the timing rather than the title.
    """
    deadline = time.monotonic() + timeout
    pane = None
    while time.monotonic() < deadline:
        pane = next((item for item in panes(boxes, box) if item["pane_id"] == pane_id), None)
        if pane and pane.get("terminal_title"):
            return pane["terminal_title"]
        time.sleep(1.0)
    raise AssertionError(f"{box}:{pane_id} never reported a terminal title in {timeout}s: {pane}")


def pane_run(boxes, box: str, pane_id: str, command: str, timeout: float = 60.0) -> None:
    """Run `command` in a pane, waiting out a peer that is momentarily unreachable.

    The counterpart to [`wait_for_output`]'s tolerance, for the same reason: driving a
    peer view crosses to the other machine, and a control channel that is reconnecting
    refuses with `unavailable` rather than queuing. Retried rather than asserted so a
    scenario about something else does not fail on the reconnect.
    """
    deadline = time.monotonic() + timeout
    last = ""
    while time.monotonic() < deadline:
        attempt = boxes.ssh(box, f"herdr pane run {pane_id} {command}", expect=(0, 1))
        if attempt.returncode == 0:
            return
        last = attempt.stderr.strip()
        time.sleep(1.0)
    raise AssertionError(f"{box}:{pane_id} never accepted a command within {timeout}s:\n{last}")


def wait_for_output(boxes, box: str, pane_id: str, needle: str, timeout: float = 30.0) -> str:
    """Poll a pane until `needle` shows up on it.

    A read that *fails* is a "not yet", not a failure. Reading a peer view goes to the
    other machine, and while that peer's control channel is reconnecting the CLI answers
    `{"code": "unavailable", "message": "peer 'box2' is reconnecting"}` and exits 1 —
    which is exactly the window a poll exists to wait out. Asserting exit 0 on every
    attempt turned that into a hard failure roughly one run in ten, and the traceback
    pointed at the read rather than at the reconnect behind it.

    Only the timeout is fatal, and it reports whatever the last attempt said, so a read
    that is failing for a real reason still explains itself.
    """
    deadline = time.monotonic() + timeout
    last = ""
    while time.monotonic() < deadline:
        # 1 is how the CLI reports a refusal; anything else is a broken invocation.
        attempt = boxes.ssh(
            box, f"herdr pane read {pane_id} --source visible --format text", expect=(0, 1)
        )
        if attempt.returncode == 0:
            last = attempt.stdout
            if needle in last:
                return last
        else:
            last = attempt.stderr.strip()
        time.sleep(1.0)
    raise AssertionError(f"{needle!r} never appeared in {box}:{pane_id} within {timeout}s:\n{last}")
