"""box1 peering to box2: the whole flow, container to container."""

from __future__ import annotations

import pytest

from _boxes import pane_run, panes, wait_for_output, wait_pane_title, wait_peer_connected

MARKER = "cross-machine-marker"


@pytest.mark.timeout(600)
def test_an_ssh_peer_enumerates_opens_and_round_trips_a_command(box_servers):
    boxes = box_servers
    boxes.ssh("box2", "herdr workspace create --label box2-ws")

    boxes.ssh("box1", "herdr peer add box2 --ssh box2 --yes")
    peer = wait_peer_connected(boxes, "box1", "box2")

    assert peer["target"] == {"destination": "box2", "type": "ssh"}, peer
    assert [ws["label"] for ws in peer["workspaces"]] == ["box2-ws"], peer
    # The peer's ids arrive namespaced by its instance id, which is what keeps two
    # servers' `w1` apart.
    remote_ws = peer["workspaces"][0]["workspace_id"]
    assert remote_ws.startswith(f"{peer['instance_id']}:")

    boxes.ssh("box1", f"herdr peer open {remote_ws} --peer box2 --focus")
    view = [pane for pane in panes(boxes, "box1") if pane.get("peer") == "box2"]
    assert len(view) == 1, view

    pane_run(boxes, "box1", view[0]["pane_id"], "'echo {}; uname -n'".format(MARKER))

    # Read it back on the machine that ran it. `uname -n` is the assertion that matters:
    # a local fallback would print this host's name. The shell prompt says `box2` too,
    # so only the line the command itself printed counts.
    remote_pane = panes(boxes, "box2")[0]["pane_id"]
    screen = wait_for_output(boxes, "box2", remote_pane, MARKER)
    lines = [line.strip() for line in screen.splitlines()]
    printed = lines.index(MARKER)
    assert lines[printed + 1] == "box2", screen

    # And it must be readable through the peer view, from box1.
    assert MARKER in wait_for_output(boxes, "box1", view[0]["pane_id"], MARKER)

    # The pane's title is the remote shell, not a local one.
    assert wait_pane_title(boxes, "box1", view[0]["pane_id"]) == "herdr@box2:~"
