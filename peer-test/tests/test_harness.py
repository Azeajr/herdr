"""The harness's own preconditions.

If these fail, nothing else in the suite means anything: every other scenario reads
a lab's state or a client's screen and believes what it is told.
"""

from __future__ import annotations

import os
import time
from pathlib import Path


def test_a_lab_boots_two_peered_servers(peer_lab):
    status = peer_lab.status()

    assert status["instances"]["a"]["running"], status
    assert status["instances"]["b"]["running"], status

    peers = peer_lab.peers("a")
    assert [peer["name"] for peer in peers] == ["b"], peers
    # `a` knows about `b`; the wiring is one-way, so `b` must not have gained one.
    assert peer_lab.peers("b") == []


def test_a_client_reaches_the_app_past_onboarding(peer_lab):
    opened = peer_lab.ui_open("a", "A")

    assert opened["instance"] == "a"
    # A fresh lab's first client sits in onboarding, which returns from `handle_mouse`
    # before any chrome hit test. `ui_open` clears it; assert it actually cleared.
    assert peer_lab.run("ui", "screen", "A")["gate"] is None
    assert peer_lab.sees("A", "spaces"), peer_lab.screen("A")


def test_a_stray_file_in_tmp_neither_breaks_the_lab_nor_gets_reaped(peer_lab):
    """`/tmp` is shared, and the lab reaps through a glob other things land in.

    The one that bit: a failing Rust peer test panics before its cleanup line and strands
    `herdr-peer-<test>-<pid>-<nanos>-client.sock`. `bridge_socket_dirs` read the
    nanoseconds as a pid, `os.kill` raised `OverflowError` rather than an `OSError`, and
    nothing caught it — every `destroy` in the suite failed at once, from one loose file
    nobody noticed. 27 teardown errors, none of them about the thing under test.
    """
    stray = Path(f"/tmp/herdr-peer-harness-decoy-{os.getpid()}-{time.time_ns()}-client.sock")
    stray.touch()
    try:
        status = peer_lab.status()
        assert status["instances"]["a"]["running"], status
        # Not a bridge dir, so it is not listed — and not deleted either: reaping a path
        # this lab did not create would be worse than the crash it replaced.
        assert str(stray) not in [entry["path"] for entry in status["bridge_dirs"]], status
        assert stray.exists()
    finally:
        stray.unlink(missing_ok=True)
