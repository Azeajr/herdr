"""The negative half of peer discovery.

box3 deliberately has no herdr binary. A real second machine cannot give you this case
without uninstalling something, which is why it is a container.
"""

from __future__ import annotations

import pytest


@pytest.mark.timeout(300)
def test_a_box_without_herdr_is_refused_and_registers_nothing(box_servers):
    boxes = box_servers

    refused = boxes.ssh("box1", "herdr peer add box3 --ssh box3 --yes", expect=1)

    output = refused.stdout + refused.stderr
    assert "is not installed" in output, output
    # Batch mode cannot approve an install, and the message has to say so rather than
    # failing as if the box were unreachable.
    assert "interactive terminal" in output, output

    # A refused add must leave no half-registered peer behind.
    listing = boxes.herdr_json("box1", "herdr peer list --json")
    assert listing["result"]["peers"] == [], listing


@pytest.mark.timeout(300)
def test_box3_really_has_no_herdr(boxes):
    """The premise of the test above, asserted rather than assumed."""
    probe = boxes.ssh("box3", "command -v herdr || echo none")

    assert probe.stdout.strip() == "none", probe.stdout

    # And nowhere on the filesystem either, not merely off `PATH`. This catches an
    # accidental binary mount while leaving the `command -v` check above green.
    found = boxes.ssh("box3", "find / -type f -name herdr 2>/dev/null || true")

    assert found.stdout.strip() == "", (
        "box3 has a herdr binary on disk; something mounted it in:\n" + found.stdout
    )
