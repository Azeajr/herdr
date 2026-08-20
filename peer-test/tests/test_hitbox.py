"""Clicking a control by name, from the coordinates herdr itself computed.

`--text` reaches most controls, but it addresses the *screen*: a label that repeats,
or a menu drawn at wherever the pointer was, has to be counted out by hand. A counted
column that misses lands on blank space, which dismisses the menu and dispatches
nothing — the failure that already cost one full false-bug investigation.

The dump closes that: the rects come from `HERDR_HITBOX_DUMP` on the server that drew
the frame, which is the same process that hit-tests the click when it arrives.
"""

from __future__ import annotations

import pytest

from conftest import EXIT_ASSERT, PEER_HEADER


@pytest.fixture
def solo_ui(lab_factory):
    """One server with a client on it. No peer — most of this is plain chrome."""
    lab = lab_factory(instances="a")
    lab.ui_open("a", "A")
    return lab


def test_every_control_is_on_screen_and_its_click_point_is_inside_it(solo_ui):
    dump = solo_ui.hitbox("A")
    controls = {entry["name"]: entry for entry in dump["controls"]}

    assert {"sidebar", "tab_bar", "terminal", "workspace[0]", "tab[0]"} <= set(controls)
    for entry in dump["controls"]:
        rect, click = entry["rect"], entry["click"]
        # An empty rect is not clickable, and offering one would let a scenario
        # "find" a control that is not drawn and click whatever is underneath.
        assert rect["width"] > 0 and rect["height"] > 0, entry
        assert rect["x"] <= click["col"] < rect["x"] + rect["width"], entry
        assert rect["y"] <= click["row"] < rect["y"] + rect["height"], entry
        assert click["row"] < dump["screen"]["height"], entry


def test_a_context_menu_item_is_clickable_by_its_label(solo_ui):
    """The case `--text` is worst at: a menu drawn wherever the pointer happened to be."""
    solo_ui.click("A", control="workspace[0]", button="right")

    menu = solo_ui.control("A", "Rename")["control"]
    # Right-clicked at the row's centre, so the menu opens there and nowhere a
    # fixed coordinate would have guessed.
    assert menu["name"] == "menu[0]", menu
    solo_ui.click("A", control="Rename")

    # The mode is the client's own answer to "did that click dispatch", which is
    # the question a blank-cell miss silently answers wrong.
    assert solo_ui.hitbox("A")["mode"] == "RenameWorkspace"
    solo_ui.keys("A", "Escape")
    assert solo_ui.hitbox("A")["mode"] != "RenameWorkspace"


def test_the_launcher_menu_is_addressable_by_label(solo_ui):
    solo_ui.click("A", control="sidebar.launcher")

    labels = [entry.get("label") for entry in solo_ui.hitbox("A")["controls"]]
    assert "keybinds" in labels, labels
    solo_ui.click("A", control="keybinds")

    assert solo_ui.hitbox("A")["mode"] == "KeybindHelp"


def test_a_control_that_is_not_drawn_fails_instead_of_clicking_blank_space(solo_ui):
    """The whole point: no menu is open, so there is nothing to hit."""
    result = solo_ui.control("A", "menu[0]", timeout=1.0, expect=EXIT_ASSERT)

    assert "no control 'menu[0]'" in result.payload["error"], result.payload
    # The refusal has to say what *is* there, or the next step is guessing again.
    assert "sidebar" in result.payload["controls"]


def test_a_peer_header_is_addressed_by_handle_not_by_its_chevron(peer_ui):
    """`--text ▾` matches any group's chevron; `peer[b]` names one peer."""
    header = peer_ui.control("A", "peer[b]")["control"]

    assert header["label"] == "b", header
    assert peer_ui.find("A", PEER_HEADER)[0]["row"] == header["rect"]["y"]

    peer_ui.click("A", control="peer[b]", button="right")
    assert peer_ui.sees("A", "Collapse"), peer_ui.screen("A")
