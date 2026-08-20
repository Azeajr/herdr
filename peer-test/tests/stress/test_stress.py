"""Bounds the stress harness holds the server to.

These are not the measurements — those live in `scripts/stress.py` and produce numbers,
which belong in a report rather than in an assertion that fails on a busy machine. What
is asserted here is what must be true at *any* speed: admission caps hold, resources
come back, and a burst that the server is supposed to absorb does not stop it answering.

Kept out of `just test-e2e` on purpose: they open 256 connections and push megabytes
through a pty, which is not what a scenario suite should do on every run. `just
test-stress` is the entry point.
"""

from __future__ import annotations

import time

import pytest

import _stress

#: `MAX_CONCURRENT_API_CONNECTIONS` in `src/api/server.rs`. The test drives well past
#: it, so a drift downward in the source would still be caught by the assertions below.
API_CONNECTION_CAP = 128


def instance(lab, name: str) -> tuple[str, int]:
    """The socket path and server pid of one lab instance."""
    entry = lab.status()["instances"][name]
    assert entry["running"] and entry["pid"], entry
    return entry["sock"], int(entry["pid"])


def test_profiler_windows_parse_including_empty_sections():
    # The idle server writes `durations=` with nothing after it, and a parser that
    # cannot read that line reports an idle server as a server that never ran.
    idle = (
        '2026-08-17T01:55:07.415646Z  INFO herdr::render_prof: render profiler window '
        'event="render.prof" window_ms=1007 counters=loop.tick=4,loop.wake.timer=4 '
        "durations= histograms=loop.active=count:4 avg_us:235 p50_us:250 p95_us:396 "
        "p99_us:396 max_us:396 gauges=queue.api.depth=last:0 max:0 samples:4"
    )
    window = _stress.ProfWindow.parse(idle)

    assert window is not None
    assert window.counters == {"loop.tick": 4, "loop.wake.timer": 4}
    assert window.durations == {}
    assert window.histograms["loop.active"]["max_us"] == 396
    assert window.gauges["queue.api.depth"]["max"] == 0
    assert window.at > 0


def test_percentiles_aggregate_to_the_worst_window_not_the_mean():
    # Averaging percentiles across windows would hide the one window that stalled,
    # which is the only window any of these workloads is looking for.
    def window(p99: int, count: int) -> _stress.ProfWindow:
        return _stress.ProfWindow(
            at=0.0,
            window_ms=1000,
            counters={},
            durations={},
            histograms={"loop.active": {"count": count, "avg_us": 100, "p99_us": p99, "max_us": p99}},
            gauges={},
        )

    report = _stress.aggregate([window(500, 10), window(40_000, 1), window(500, 10)])

    assert report["histograms"]["loop.active"]["p99_worst_us"] == 40_000
    assert report["histograms"]["loop.active"]["max_us"] == 40_000
    assert report["histograms"]["loop.active"]["count"] == 21


def test_api_flood_past_the_cap_is_refused_and_leaves_nothing_behind(lab_factory):
    lab = lab_factory(instances="a")
    lab.cli("a", "workspace", "create", "--label", "flood")
    sock, pid = instance(lab, "a")
    before = _stress.Resources.sample(pid)

    with _stress.ResourceWatch(pid, interval=0.05) as watch:
        flood = _stress.api_flood(sock, "pane.list", {}, concurrency=256, rounds=2)

    # Every connection is answered one way or the other: served, told the server is
    # overloaded, or dropped at the listener. What must not happen is the server
    # accepting all of them and spawning a thread each.
    assert flood["ok"] + flood["failed"] == flood["sent"]
    assert flood["overloaded"] + flood["failed"] > 0, flood
    assert watch.peak.threads <= before.threads + API_CONNECTION_CAP + 8, watch.as_dict()

    # And it is still a working server afterwards, holding no threads or descriptors
    # from the flood.
    assert _stress.wait_until(
        lambda: _stress.Resources.sample(pid).threads <= before.threads + 2, timeout=30.0
    ), _stress.Resources.sample(pid).threads
    assert _stress.Resources.sample(pid).fds <= before.fds + 2
    assert _stress.api_request(sock, "pane.list", {}, timeout=5.0).ok


@pytest.mark.timeout(600)
def test_a_pasted_burst_into_a_stalled_pane_keeps_the_server_answering(lab_factory):
    """800 KB pasted into a pane whose child never reads it.

    The pty stops accepting, so the bytes have nowhere to go; the server still has to
    answer everyone else. The same bytes *typed* rather than pasted currently take tens
    of seconds, which is recorded in the plan as its own finding — this gate is for the
    path that batches, so a regression that makes pasting behave like typing fails here.
    """
    lab = lab_factory(instances="a")
    created = lab.cli("a", "workspace", "create", "--label", "stalled")
    pane = created["result"]["root_pane"]["pane_id"]
    sock, _pid = instance(lab, "a")
    lab.ui_open("a", "A")

    # A child that never reads its stdin, so the pty's input buffer stays full.
    lab.cli("a", "pane", "send-text", pane, "sleep 600\n")
    lab.wait_for("A", "sleep 600")

    # Driven through tmux directly: 200 `lab.py ui text` calls would spend more time in
    # interpreter startup than in the server.
    payload = "x" * 4096
    for _ in range(200):
        _paste(lab, "A", payload)

    started = time.monotonic()
    assert _stress.wait_until(
        lambda: _stress.api_request(sock, "pane.list", {}, timeout=2.0).ok,
        timeout=120.0,
        interval=0.5,
    ), "server never answered again after a pasted burst"
    assert time.monotonic() - started < 15.0, "server took too long to answer after a paste"


@pytest.mark.timeout(600)
def test_a_typed_burst_into_a_stalled_pane_does_not_freeze_the_server(lab_factory):
    """400 KB of typed input, which is the path B6 was about.

    Typed input is not batched by the terminal the way a paste is, so it is the client
    that has to join it — a regression to one socket message per character put the
    server past 27 seconds of silence for this much input, against under 4 with the
    join in place. The threshold sits between the two.
    """
    lab = lab_factory(instances="a")
    created = lab.cli("a", "workspace", "create", "--label", "stalled")
    pane = created["result"]["root_pane"]["pane_id"]
    sock, _pid = instance(lab, "a")
    lab.ui_open("a", "A")

    lab.cli("a", "pane", "send-text", pane, "sleep 600\n")
    lab.wait_for("A", "sleep 600")

    payload = "x" * 4096
    for _ in range(100):
        _type(lab, "A", payload)

    started = time.monotonic()
    assert _stress.wait_until(
        lambda: _stress.api_request(sock, "pane.list", {}, timeout=2.0).ok,
        timeout=180.0,
        interval=0.5,
    ), "server never answered again after a typed burst"
    assert time.monotonic() - started < 15.0, "typed input is being sent unbatched again"


def _type(lab, client: str, text: str) -> None:
    import subprocess

    subprocess.run(
        ["tmux", "-L", f"hl-{lab.name}", "send-keys", "-t", client, "-l", "--", text],
        capture_output=True,
        text=True,
        check=True,
    )


def _paste(lab, client: str, text: str) -> None:
    import subprocess

    buffer = f"stress-{client}"
    subprocess.run(
        ["tmux", "-L", f"hl-{lab.name}", "load-buffer", "-b", buffer, "-"],
        input=text,
        capture_output=True,
        text=True,
        check=True,
    )
    subprocess.run(
        ["tmux", "-L", f"hl-{lab.name}", "paste-buffer", "-p", "-b", buffer, "-t", client],
        capture_output=True,
        text=True,
        check=True,
    )
