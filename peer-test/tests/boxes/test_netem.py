"""What a peer does when the link between the two boxes is bad.

`tc netem` shapes only the traffic addressed to the *other box* — band 3 of a `prio`
qdisc, reachable only through a destination filter — so the ssh these scenarios observe
through keeps its normal path even at 100% loss. Unscoped shaping would take the observer
down with the subject, and a test that cannot see the box it broke cannot tell a peer
that failed to recover from an assertion that failed to run.

Nothing else in the suite exercises a degraded link: every other scenario runs on a
loopback bridge where a round trip costs nothing and a packet is never dropped.
"""

from __future__ import annotations

import re
import time

import pytest

import _boxes
from _boxes import pane_run, panes, wait_for_output, wait_peer_connection, wait_peer_connected

#: A round trip and a loss rate a real link between two machines can have. Applied to
#: box2's replies only, so box1 sees a 200ms RTT to its peer.
DELAY = "200ms"
LOSS = "1%"

BEFORE = "before-partition"
AFTER = "after-heal"
SLOW = "slow-link-marker"
GONE = "gone-marker"

#: Enough logging to count what a view did after the peer refused it, and nothing else.
#: The bare `herdr=info` default hides the refusals; `herdr=debug` buries them under a
#: render line per tick.
VIEW_LOG = "herdr=info,herdr::terminal::remote=debug"


def open_peer_view(boxes, workspace_id: str) -> str:
    """Open box2's workspace on box1 and return the local pane backing the view."""
    boxes.ssh("box1", f"herdr peer open {workspace_id} --peer box2 --focus")
    view = [pane for pane in panes(boxes, "box1") if pane.get("peer") == "box2"]
    assert len(view) == 1, view
    return view[0]["pane_id"]


@pytest.mark.timeout(600)
def test_a_peer_connects_enumerates_and_round_trips_over_a_slow_lossy_link(box_servers, netem):
    boxes = box_servers
    netem.apply("box2", DELAY, LOSS, to="box1")

    boxes.ssh("box2", "herdr workspace create --label slow-ws")
    boxes.ssh("box1", "herdr peer add box2 --ssh box2 --yes")

    # Every step of the handshake — resolving the remote binary, standing up the ssh
    # bridge, identifying, subscribing, enumerating — pays the latency, and the ping
    # behind `identify` gives up after 5s. That the peer arrives at all is the claim.
    peer = wait_peer_connected(boxes, "box1", "box2", timeout=180.0)
    assert [ws["label"] for ws in peer["workspaces"]] == ["slow-ws"], peer

    view = open_peer_view(boxes, peer["workspaces"][0]["workspace_id"])
    pane_run(boxes, "box1", view, "'echo {}; uname -n'".format(SLOW))

    # Read it on the machine that ran it: a local fallback would print this box's name.
    remote_pane = panes(boxes, "box2")[0]["pane_id"]
    screen = wait_for_output(boxes, "box2", remote_pane, SLOW, timeout=60.0)
    lines = [line.strip() for line in screen.splitlines()]
    assert lines[lines.index(SLOW) + 1] == "box2", screen

    assert SLOW in wait_for_output(boxes, "box1", view, SLOW, timeout=60.0)


@pytest.mark.timeout(900)
def test_a_peer_and_its_open_view_recover_when_a_partition_heals(box_servers, netem):
    boxes = box_servers
    boxes.ssh("box2", "herdr workspace create --label partition-ws")
    boxes.ssh("box1", "herdr peer add box2 --ssh box2 --yes")
    connected = wait_peer_connected(boxes, "box1", "box2")

    # Opened *before* the break, so what heals is a view that already exists rather than
    # a fresh one — which is the case a reconnect could quietly fail to restore.
    view = open_peer_view(boxes, connected["workspaces"][0]["workspace_id"])
    pane_run(boxes, "box1", view, f"'echo {BEFORE}'")
    wait_for_output(boxes, "box1", view, BEFORE)

    netem.partition("box2", to="box1")

    # No transport error surfaces on its own: the event stream simply goes quiet, so the
    # 15s heartbeat is what notices, and its ping is bounded at 5s.
    lost = wait_peer_connection(boxes, "box1", "box2", "reconnecting", timeout=120.0)
    assert lost["stale"] is True, lost
    assert lost["error"], lost
    # The peer's last known workspaces are kept, marked stale rather than dropped: a
    # partition is not the peer saying it has nothing.
    assert [ws["label"] for ws in lost["workspaces"]] == ["partition-ws"], lost
    # The local view is still there to be restored.
    assert [pane["pane_id"] for pane in panes(boxes, "box1") if pane.get("peer") == "box2"] == [view]

    netem.clear("box2")

    healed = wait_peer_connected(boxes, "box1", "box2", timeout=180.0)
    assert healed["stale"] is False, healed
    # The same server, so nothing about the peer's state had to be thrown away.
    assert healed["instance_id"] == connected["instance_id"], (connected, healed)

    # Green is not the claim; usable is. The pane that predates the partition still
    # drives the shell on box2, and its scrollback survived.
    pane_run(boxes, "box1", view, f"'echo {AFTER}'")
    screen = wait_for_output(boxes, "box1", view, AFTER, timeout=60.0)
    assert BEFORE in screen, screen

    remote_pane = panes(boxes, "box2")[0]["pane_id"]
    assert AFTER in wait_for_output(boxes, "box2", remote_pane, AFTER, timeout=60.0)


@pytest.mark.timeout(600)
def test_a_pane_closed_on_the_peer_retires_its_view_without_a_second_attempt(box_servers, netem):
    """A view whose pane died on the peer goes on the peer's *first* answer.

    "not found" for a pane id is authoritative — the peer never reuses those ids while
    it lives — so asking again cannot change it, and on a shaped link each extra ask is
    a whole round trip with the pane gray in the meantime. Counted rather than timed:
    the number of times the view went back is what the behaviour is, and it does not
    move with the latency the way a stopwatch does.
    """
    boxes = box_servers
    # box1 restarted with the view log on, because only its own log says how many times
    # it went back. Replacing the fixture's server rather than adding to it also means
    # nothing from before this point is in the file.
    _boxes.stop_and_wipe(boxes, "box1")
    _boxes.start_server(boxes, "box1", log=VIEW_LOG)
    _boxes.wait_ready(boxes, "box1")

    boxes.ssh("box2", "herdr workspace create --label gone-ws")
    boxes.ssh("box1", "herdr peer add box2 --ssh box2 --yes")
    peer = wait_peer_connected(boxes, "box1", "box2")

    view = open_peer_view(boxes, peer["workspaces"][0]["workspace_id"])
    pane_run(boxes, "box1", view, f"'echo {GONE}'")
    wait_for_output(boxes, "box1", view, GONE)

    # Shaped only now, so the peering above does not pay for it: what the delay buys is
    # a link where a wasted reconnect is expensive, which is the reason to have none.
    netem.apply("box2", DELAY, LOSS, to="box1")

    remote_pane = panes(boxes, "box2")[0]["pane_id"]
    started = time.monotonic()
    boxes.ssh("box2", f"herdr pane close {remote_pane}")

    deadline = time.monotonic() + 120.0
    while time.monotonic() < deadline:
        if not [pane for pane in panes(boxes, "box1") if pane.get("peer") == "box2"]:
            break
        time.sleep(0.5)
    else:
        raise AssertionError(f"box1 still holds a view of a pane box2 closed:\n{panes(boxes, 'box1')}")
    elapsed = time.monotonic() - started

    # The view was the workspace's only pane, so the workspace went with it rather than
    # staying behind empty.
    assert boxes.herdr_json("box1", "herdr workspace list")["result"]["workspaces"] == []

    log = _boxes.server_log(boxes, "box1")
    refusals = re.findall(r"peer terminal shut down.*not found", log)
    assert len(refusals) == 1, (
        f"the view went back to box2 {len(refusals) - 1} time(s) after being told the "
        f"pane was gone (retired in {elapsed:.2f}s):\n" + "\n".join(refusals)
    )
    assert "retiring the view" in log, log[-2000:]
