"""The peer group header, driven with real mouse events.

Everything here is a click that becomes a client input event handled in-process, which
is the half of herdr that a CLI or API test cannot reach at all: a request is gated on
`request_targets_peer_workspace` long before a click has been hit-tested.
"""

from __future__ import annotations

from conftest import PEER_HEADER, PEER_HEADER_COLLAPSED

REFUSAL = "before hiding it"


def test_the_header_and_picker_name_the_peer_by_its_handle(peer_ui):
    """A socket peer's label is its full socket path, which identifies nothing here.

    The sidebar is ~25 columns wide, so the label truncated to `/tmp/hl-xxxxx/b…` — the
    peer was unidentifiable even though its handle is `b`.
    """
    # Only the sidebar half of the line: the right half is the terminal pane.
    sidebar = peer_ui.peer_header("A").split("│")[0]

    assert sidebar.split() == [PEER_HEADER, "b", "0/1", "●"], sidebar

    peer_ui.click("A", text=PEER_HEADER)
    peer_ui.wait_for("A", "open workspace on b")
    assert not peer_ui.sees("A", "herdr.sock"), peer_ui.screen("A")


def test_left_clicking_the_header_opens_the_peer_workspace_picker(peer_ui):
    assert "0/1" in peer_ui.peer_header("A")

    peer_ui.open_peer_workspace("A", "remote-ws")

    # The peer's workspace is now open here, which the header's count is the statement of.
    peer_ui.wait_for("A", "1/1")
    pane = next(pane for pane in peer_ui.state("a")["panes"] if pane.get("peer") == "b")
    assert pane["workspace_id"] != "w1", pane
    # Opening a view of a peer workspace must not have created anything on the peer.
    assert len(peer_ui.state("b")["panes"]) == 1


def test_right_clicking_the_header_collapses_and_expands_the_group(peer_ui):
    peer_ui.open_peer_workspace("A", "remote-ws")
    peer_ui.wait_for("A", "1/1")
    # A collapsed group still shows the workspace currently in view, so collapse is only
    # observable from a local workspace. Click the local one first.
    peer_ui.click("A", text="· ~")

    peer_ui.open_peer_menu("A")
    peer_ui.click("A", text="Collapse")

    peer_ui.wait_for("A", PEER_HEADER_COLLAPSED)
    assert not peer_ui.sees("A", "b:w1:p1"), peer_ui.screen("A")

    peer_ui.open_peer_menu("A", chevron=PEER_HEADER_COLLAPSED)
    # The item is the inverse of the current state, which is the only thing that says
    # the menu was built from this peer's collapse state and not a default.
    assert peer_ui.sees("A", "Expand"), peer_ui.screen("A")
    peer_ui.click("A", text="Expand")

    peer_ui.wait_for("A", PEER_HEADER)
    assert peer_ui.sees("A", "b:w1:p1"), peer_ui.screen("A")


def test_hiding_a_peer_with_an_open_workspace_is_refused_and_the_refusal_expires(peer_ui):
    """Guards `de82729b`: the refusal had no deadline, so it outlived its own condition."""
    peer_ui.open_peer_workspace("A", "remote-ws")
    peer_ui.wait_for("A", "1/1")

    peer_ui.open_peer_menu("A")
    # The message expires after 5s, and each lab.py invocation costs about a second, so
    # do not let the click settle before looking for it.
    peer_ui.click("A", text="Hide for session", settle=0.2)

    peer_ui.wait_for("A", REFUSAL, timeout=4)
    assert PEER_HEADER in peer_ui.peer_header("A"), peer_ui.screen("A")

    peer_ui.wait_gone("A", REFUSAL, timeout=15)


def test_hiding_permanently_with_an_open_workspace_writes_nothing(peer_ui):
    peer_ui.open_peer_workspace("A", "remote-ws")
    peer_ui.wait_for("A", "1/1")

    peer_ui.open_peer_menu("A")
    peer_ui.click("A", text="Hide permanently", settle=0.2)

    peer_ui.wait_for("A", REFUSAL, timeout=4)
    # A refused hide must not reach the config file, or the peer would come back hidden
    # at the next cold start with no workspace left to explain why.
    assert "[peer_hidden]" not in peer_ui.config_text("a")
    peer_ui.wait_gone("A", REFUSAL, timeout=15)

    # Close the peer workspace and the same action now succeeds.
    peer_ui.cli("a", "workspace", "close", "w2")
    peer_ui.wait_for("A", "0/1")
    peer_ui.open_peer_menu("A")
    peer_ui.click("A", text="Hide permanently")

    peer_ui.wait_gone("A", PEER_HEADER)
    assert "[peer_hidden]" in peer_ui.config_text("a")
