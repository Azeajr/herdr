"""What a server believes the second time it boots.

`477dca2d` fixed `[peer_hidden]` and `[peer_history]` being written by the UI but never
read back at cold start, so a permanently-hidden peer returned on every restart. The unit
tests that shipped with it call `App::new` directly; nothing exercised a server process
that exits and starts again off its own config file. These do.
"""

from __future__ import annotations

import re

from conftest import PEER_HEADER


def hide_peer_permanently(lab, client: str) -> None:
    lab.open_peer_menu(client)
    lab.click(client, text="Hide permanently")


def test_a_permanently_hidden_peer_stays_hidden_across_a_restart(peer_ui):
    hide_peer_permanently(peer_ui, "A")

    config = peer_ui.config_text("a")
    assert "[peer_hidden]" in config, config
    assert '"b"' in config, config
    peer_ui.wait_gone("A", PEER_HEADER)

    peer_ui.restart(instances="a,b")
    peer_ui.ui_open("a", "A")
    # The header is missing for a second or two after any restart, so wait for the peer's
    # workspace list to arrive before believing that its absence means anything.
    peer_ui.wait_peer_enumerated("a", "b")

    # Revert 477dca2d and the header is back here: the config says hidden, but nothing
    # read the config.
    assert peer_ui.peer_header("A") == "", peer_ui.screen("A")
    # Hiding is a display decision, not a removal — the peer must still be registered.
    assert [peer["name"] for peer in peer_ui.peers("a")] == ["b"]
    # And it must still be reachable: the global menu only offers "unhide peers" when
    # `hidden_peers_config` is non-empty, which is the loaded value itself.
    peer_ui.click("A", text="menu")
    assert peer_ui.sees("A", "unhide peers"), peer_ui.screen("A")


def test_a_session_hide_never_reaches_the_config_and_is_reversible(peer_ui):
    """The other half of the pair: session hides are session state, not config state.

    They do survive a restart — the session snapshot carries `hidden_peers` — so the
    thing that separates them from a permanent hide is that they never write the file,
    and that the picker can take them back.
    """
    peer_ui.open_peer_menu("A")
    peer_ui.click("A", text="Hide for session")
    peer_ui.wait_gone("A", PEER_HEADER)

    assert "[peer_hidden]" not in peer_ui.config_text("a")

    peer_ui.click("A", text="menu")
    peer_ui.click("A", text="unhide peers")
    peer_ui.wait_for("A", "hidden peers")
    # The picker names the peer by its handle, which is the one place a socket peer is
    # identifiable in the UI today.
    assert peer_ui.sees("A", "b (session)"), peer_ui.screen("A")

    peer_ui.keys("A", "Enter")
    peer_ui.wait_for("A", PEER_HEADER)


def test_peer_history_is_capped_and_deduped_by_target(lab_factory):
    """Twelve real adds through the server, not twelve calls to the upsert function.

    The string-level rules are unit-tested; what is not, is that every successful
    `peer.add` reaches `record_peer_history` and that the file survives the round trip.
    """
    lab = lab_factory(instances="a")

    # A peer's socket never has to answer for the registry to accept it, so twelve
    # distinct targets cost nothing but twelve names.
    for index in range(12):
        lab.cli("a", "peer", "add", f"h{index}", "--socket", f"{lab.root}/fake{index}.sock", "--yes")

    entries = peer_history_entries(lab.config_text("a"))
    assert len(entries) == 10, entries
    # Newest first, and the two oldest have fallen off.
    assert [name for name, _ in entries] == [f"h{index}" for index in range(11, 1, -1)]

    # Re-adding a target already in history moves it to the front instead of duplicating
    # it — the dedupe key is the target, not the name.
    lab.cli("a", "peer", "remove", "h5")
    lab.cli("a", "peer", "add", "h5-again", "--socket", f"{lab.root}/fake5.sock", "--yes")

    entries = peer_history_entries(lab.config_text("a"))
    assert len(entries) == 10, entries
    assert entries[0][0] == "h5-again"
    assert [target for _, target in entries].count(f"socket://{lab.root}/fake5.sock") == 1


def peer_history_entries(config: str) -> list[tuple[str, str]]:
    """`[peer_history] recent` as (name, target) pairs, in file order.

    Parsed with a regex rather than a TOML library so the suite keeps zero runtime
    dependencies; the value herdr writes is a single inline-table array.
    """
    section = config.partition("[peer_history]")[2].partition("\n[")[0]
    return re.findall(r'name\s*=\s*"([^"]*)"\s*,\s*target\s*=\s*"([^"]*)"', section)
