"""Pane actions inside a peer-backed workspace.

A split here has to land twice: a view on `a`, and the process that actually runs on
`b`. A test that only looks at one of them cannot tell a routed split from a local one
that merely looks right on screen.
"""

from __future__ import annotations

import re


def peer_panes(lab, instance: str, peer: str) -> list[dict]:
    return [pane for pane in lab.state(instance)["panes"] if pane.get("peer") == peer]


def test_context_menu_split_right_lands_on_both_servers(peer_ui):
    peer_ui.open_peer_workspace("A", "remote-ws")
    peer_ui.wait_for("A", "1/1")

    view = peer_panes(peer_ui, "a", "b")
    assert len(view) == 1, view
    assert view[0]["peer_view"]["state"] == "connected", view[0]
    before = len(peer_ui.state("b")["panes"])

    # `--pane` clicks the centre of the API's own rect for the pane, so the menu opens
    # over the pane rather than wherever a hand-counted column happened to point.
    peer_ui.click("A", pane=view[0]["pane_id"], button="right")
    assert peer_ui.sees("A", "Split right"), peer_ui.screen("A")
    peer_ui.click("A", text="Split right", settle=3.0)

    after_view = peer_panes(peer_ui, "a", "b")
    assert len(after_view) == 2, after_view
    # The half that matters: the new process exists on the peer, not here.
    assert len(peer_ui.state("b")["panes"]) == before + 1


def test_a_peer_backed_pane_runs_its_command_on_the_peer(peer_ui):
    peer_ui.open_peer_workspace("A", "remote-ws")
    peer_ui.wait_for("A", "1/1")

    view = peer_panes(peer_ui, "a", "b")[0]
    marker = "peer-pane-marker"
    peer_ui.cli("a", "pane", "run", view["pane_id"], f"echo {marker}")

    peer_ui.wait_for("A", marker)
    # The output came back through the peer view, so it must also be readable on the
    # server that actually ran it.
    remote = peer_ui.state("b")["panes"][0]["pane_id"]
    # `pane read --format text` prints the screen, not JSON, so lab.py hands it back
    # under `raw`.
    read = peer_ui.cli("b", "pane", "read", remote, "--source", "visible", "--format", "text")
    assert marker in read["raw"], read.payload


def test_peer_text_query_runs_on_the_server_that_owns_scrollback(peer_ui):
    peer_ui.open_peer_workspace("A", "remote-ws")
    peer_ui.wait_for("A", "1/1")

    view = peer_panes(peer_ui, "a", "b")[0]
    remote = peer_ui.state("b")["panes"][0]["pane_id"]
    marker = "peer-search-authority"
    peer_ui.cli("b", "pane", "run", remote, "echo", marker, "second-word")
    peer_ui.cli("b", "pane", "wait-output", remote, "--regex", marker, "--timeout", "10000")

    response = peer_ui.api(
        "a",
        "pane.text_query",
        {
            "pane_id": view["pane_id"],
            "type": "search",
            "query": marker,
            "case_sensitive": True,
        },
    )
    matches = response["result"]["query"]["matches"]
    assert matches, response

    start = matches[-1]["start"]
    response = peer_ui.api(
        "a",
        "pane.text_query",
        {
            "pane_id": view["pane_id"],
            "type": "motion",
            "row": start["row"],
            "col": start["col"],
            "motion": "next_word_start",
        },
    )
    target = response["result"]["query"]["target"]
    assert target is not None, response
    assert target["row"] >= start["row"]


def pty_size(lab, pane: str) -> str:
    """The size `b`'s own shell reports for one of its panes.

    Asked of the shell rather than read out of herdr's metadata on either side: the
    size crossing the boundary is only interesting if the process on the far end
    actually got it.
    """
    lab.cli("b", "pane", "run", pane, "stty", "size")
    lab.cli("b", "pane", "wait-output", pane, "--regex", r"^\d+ \d+$", "--timeout", "10000")
    lines = lab.cli("b", "pane", "read", pane, "--source", "visible", "--format", "text")["raw"]
    sizes = [line.strip() for line in lines.splitlines() if re.fullmatch(r"\d+ \d+", line.strip())]
    assert sizes, lines
    return sizes[-1]


def test_a_headless_server_opens_every_peer_view_at_the_size_it_will_render(peer_lab):
    """No client on `a`, which is where the estimate used to leak across the boundary.

    A pane is created at `estimate_pane_size()` and corrected by the next render, and
    that estimate returned the pane's *outer* rect while the render sizes a pty to the
    inner one. Locally nobody could see the difference. Here the wrong size is what `a`
    hands `b` at attach, and a headless server only resizes panes while it has none laid
    out — so the *second* view kept the over-estimate until a client attached: 23x54
    against the 23x53 the first view got.
    """
    b_id = peer_lab.instance_id("b")
    for label in ("ws1", "ws2"):
        peer_lab.cli("b", "workspace", "create", "--label", label)
    for workspace in ("w1", "w2"):
        peer_lab.cli("a", "peer", "open", f"{b_id}:{workspace}:p1", "--peer", "b", "--focus")

    first, second = (pty_size(peer_lab, f"{ws}:p1") for ws in ("w1", "w2"))

    assert second == first, f"first view {first}, second {second}"
