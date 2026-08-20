"""Measurement primitives for the stress harness: profiler windows, resources, load.

`stress.py` drives labs and workloads; this module is everything that reads a number
back out, kept stdlib-only so the pytest scenarios can import it directly the way they
already import `_common`.

Three things live here because all seven workloads in the analysis need all three:

  * **Profiler windows.** `HERDR_RENDER_PROF=1` makes the server log one
    `event="render.prof"` line per second. `ProfTail` remembers a byte offset, so a
    workload measures *its own* phase rather than everything since boot.
  * **Resources.** Threads, RSS and open descriptors of the server pid. Every leak
    finding in the analysis (B3, B4, B5) is a resource that fails to come back, and
    none of them show up in a latency number.
  * **Load.** Concurrent API connections, because the API flood workload has to open
    more of them than the server admits, and a subprocess per request cannot.

## What aggregation across windows can and cannot say

The profiler logs a *summary* per window, not its buckets, so aggregating windows is
not the same as aggregating observations:

| kind | across windows | honest? |
|---|---|---|
| counter | summed | exact |
| duration/histogram `count` | summed | exact |
| duration/histogram `max` | max | exact |
| duration/histogram `avg` | count-weighted mean of per-window means | ±1 µs per window, from the log's integer truncation |
| histogram `p50/p95/p99` | **worst window**, reported as `p99_worst_us` | an upper bound on the true percentile, never an understatement |
| gauge `max` | max | exact — the profiler already retains gauge peaks across windows |

A stall that lands in one window is exactly what these workloads hunt for, so taking
the worst window rather than a blended figure is the conservative direction.
"""

from __future__ import annotations

import json
import os
import re
import socket
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

#: One profiler window as the server logs it. Message first, then the fields, with
#: `gauges` last and running to end of line.
PROF_LINE = re.compile(
    r'event="render\.prof" window_ms=(?P<window_ms>\d+)'
    r" counters=(?P<counters>.*?)"
    r" durations=(?P<durations>.*?)"
    r" histograms=(?P<histograms>.*?)"
    r" gauges=(?P<gauges>.*)$"
)


def _entries(text: str) -> dict[str, str]:
    """`a=1,b=2` to `{"a": "1", "b": "2"}`.

    Metric names are fixed `&'static str` literals with no commas or equals signs in
    them, which is what makes splitting on those characters safe here.
    """
    out: dict[str, str] = {}
    for chunk in text.split(","):
        if not chunk:
            continue
        name, _, value = chunk.partition("=")
        out[name] = value
    return out


def _timestamp(line: str) -> float:
    """Unix seconds from a tracing log line's leading ISO timestamp, 0.0 if absent."""
    try:
        return datetime.fromisoformat(line[:27].replace("Z", "+00:00")).timestamp()
    except ValueError:
        return 0.0


def _fields(value: str) -> dict[str, int]:
    """`count:4 avg_us:235 max_us:396` to a dict of ints."""
    out: dict[str, int] = {}
    for part in value.split():
        key, _, number = part.partition(":")
        try:
            out[key] = int(number)
        except ValueError:
            continue
    return out


@dataclass
class ProfWindow:
    """One second of profiler output."""

    at: float
    window_ms: int
    counters: dict[str, int]
    durations: dict[str, dict[str, int]]
    histograms: dict[str, dict[str, int]]
    gauges: dict[str, dict[str, int]]

    @classmethod
    def parse(cls, line: str) -> ProfWindow | None:
        match = PROF_LINE.search(line)
        if not match:
            return None
        return cls(
            at=_timestamp(line),
            window_ms=int(match["window_ms"]),
            counters={k: int(v) for k, v in _entries(match["counters"]).items() if v.isdigit()},
            durations={k: _fields(v) for k, v in _entries(match["durations"]).items()},
            histograms={k: _fields(v) for k, v in _entries(match["histograms"]).items()},
            gauges={k: _fields(v) for k, v in _entries(match["gauges"]).items()},
        )


class ProfTail:
    """Profiler windows appended to one server log after a marked point.

    A workload marks the log, generates load, then collects. Without the mark every
    measurement would include the server's boot and whatever the previous phase did,
    which is how a fix looks like it changed nothing.
    """

    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        self.offset = 0

    def mark(self) -> ProfTail:
        """Forget everything already written. Returns self so it can be chained."""
        self.offset = self.path.stat().st_size if self.path.is_file() else 0
        return self

    def read(self) -> list[ProfWindow]:
        """Every window logged since `mark`, leaving the offset where it was.

        Non-destructive on purpose: a workload polls this to wait for enough windows
        and then reads the same set again to report on it.
        """
        if not self.path.is_file():
            return []
        with self.path.open("r", errors="replace") as handle:
            handle.seek(self.offset)
            return [w for line in handle if (w := ProfWindow.parse(line))]

    def wait_for_windows(self, count: int, *, timeout: float = 20.0) -> list[ProfWindow]:
        """Block until `count` windows have been written, or return what arrived.

        The profiler flushes on activity rather than on a timer of its own, so an
        idle server can be slow to produce a window and a caller that slept a fixed
        number of seconds would measure an empty set.
        """
        deadline = time.monotonic() + timeout
        windows: list[ProfWindow] = []
        while time.monotonic() < deadline:
            windows = self.read()
            if len(windows) >= count:
                return windows
            time.sleep(0.2)
        return windows


def worst_loop_gap(windows: list[ProfWindow]) -> float:
    """The longest stretch between two profiler windows, in seconds.

    The profiler flushes from the top of the server's main loop, which parks for at
    most 250 ms, so windows arrive about once a second for as long as that loop is
    running at all. A gap is therefore not a quiet server — it is a loop that was not
    reached, and its length is how long the server was unresponsive.
    """
    return max(
        (later.at - earlier.at for earlier, later in zip(windows, windows[1:]) if earlier.at and later.at),
        default=0.0,
    )


def aggregate(windows: list[ProfWindow]) -> dict:
    """Fold windows into one report. See the module docstring on what each fold means."""
    counters: dict[str, int] = {}
    durations: dict[str, dict[str, int]] = {}
    histograms: dict[str, dict[str, int]] = {}
    gauges: dict[str, dict[str, int]] = {}

    for window in windows:
        for name, value in window.counters.items():
            counters[name] = counters.get(name, 0) + value
        for name, stats in window.durations.items():
            _fold_timing(durations, name, stats)
        for name, stats in window.histograms.items():
            _fold_timing(histograms, name, stats)
            for percentile in ("p50_us", "p95_us", "p99_us"):
                if percentile in stats:
                    key = percentile.replace("_us", "_worst_us")
                    entry = histograms[name]
                    entry[key] = max(entry.get(key, 0), stats[percentile])
        for name, stats in window.gauges.items():
            entry = gauges.setdefault(name, {"last": 0, "max": 0, "samples": 0})
            entry["last"] = stats.get("last", 0)
            entry["max"] = max(entry["max"], stats.get("max", 0))
            entry["samples"] += stats.get("samples", 0)

    return {
        "windows": len(windows),
        "window_ms": sum(w.window_ms for w in windows),
        "worst_loop_gap_s": round(worst_loop_gap(windows), 1),
        "counters": counters,
        "durations": durations,
        "histograms": histograms,
        "gauges": gauges,
    }


def _fold_timing(into: dict[str, dict[str, int]], name: str, stats: dict[str, int]) -> None:
    entry = into.setdefault(name, {"count": 0, "avg_us": 0, "max_us": 0, "total_us": 0})
    count = stats.get("count", 0)
    entry["count"] += count
    # Prefer the window's own total: `avg_us` is truncated to whole microseconds, so
    # folding it back loses everything about a sub-microsecond measurement. Histograms
    # report no total, hence the fallback.
    entry["total_us"] += stats.get("total_us", stats.get("avg_us", 0) * count)
    entry["max_us"] = max(entry["max_us"], stats.get("max_us", 0))
    entry["avg_us"] = entry["total_us"] // entry["count"] if entry["count"] else 0


def metric(report: dict, kind: str, name: str, field_name: str, default: int = 0) -> int:
    """One number out of an aggregate, without four lines of `.get` at every call site."""
    if kind == "counters":
        return report.get("counters", {}).get(name, default)
    return report.get(kind, {}).get(name, {}).get(field_name, default)


# ---------------------------------------------------------------------------
# resources
# ---------------------------------------------------------------------------


@dataclass
class Resources:
    """What the server is holding right now.

    Descriptors are counted rather than listed: the count is the leak signal, and
    listing them on every sample makes the sampler itself the expensive thing.
    """

    threads: int = 0
    rss_kb: int = 0
    fds: int = 0

    @classmethod
    def sample(cls, pid: int) -> Resources:
        status = Path(f"/proc/{pid}/status")
        threads = rss = 0
        if status.is_file():
            for line in status.read_text(errors="replace").splitlines():
                if line.startswith("Threads:"):
                    threads = int(line.split()[1])
                elif line.startswith("VmRSS:"):
                    rss = int(line.split()[1])
        try:
            fds = len(os.listdir(f"/proc/{pid}/fd"))
        except OSError:
            fds = 0
        return cls(threads=threads, rss_kb=rss, fds=fds)

    def delta(self, other: Resources) -> dict[str, int]:
        return {
            "threads": other.threads - self.threads,
            "rss_kb": other.rss_kb - self.rss_kb,
            "fds": other.fds - self.fds,
        }

    def as_dict(self) -> dict[str, int]:
        return {"threads": self.threads, "rss_kb": self.rss_kb, "fds": self.fds}


class ResourceWatch:
    """Peak resource use across a phase, sampled from a background thread.

    Before-and-after sampling misses the shape that matters for B5: a flood that
    spawns 128 threads and reaps them again looks identical to one that spawned none.
    """

    def __init__(self, pid: int, *, interval: float = 0.2) -> None:
        self.pid = pid
        self.interval = interval
        self.start = Resources.sample(pid)
        self.peak = self.start
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def __enter__(self) -> ResourceWatch:
        self._thread.start()
        return self

    def __exit__(self, *_exc) -> None:
        self._stop.set()
        self._thread.join(timeout=2.0)
        self.end = Resources.sample(self.pid)

    def _run(self) -> None:
        while not self._stop.wait(self.interval):
            sample = Resources.sample(self.pid)
            self.peak = Resources(
                threads=max(self.peak.threads, sample.threads),
                rss_kb=max(self.peak.rss_kb, sample.rss_kb),
                fds=max(self.peak.fds, sample.fds),
            )

    def as_dict(self) -> dict:
        end = getattr(self, "end", Resources.sample(self.pid))
        return {
            "start": self.start.as_dict(),
            "peak": self.peak.as_dict(),
            "end": end.as_dict(),
            "delta": self.start.delta(end),
        }


# ---------------------------------------------------------------------------
# load
# ---------------------------------------------------------------------------


@dataclass
class ApiResult:
    """One request's outcome. `error` carries the transport failure, not a JSON error."""

    ok: bool
    elapsed_ms: float
    payload: dict = field(default_factory=dict)
    error: str = ""

    @property
    def overloaded(self) -> bool:
        return "server_overloaded" in json.dumps(self.payload)


def api_request(sock_path: str, method: str, params: dict | None = None, *, timeout: float = 20.0) -> ApiResult:
    """One JSON-RPC request over its own connection, which is what the server expects.

    The API server reads exactly one request line per connection, so this deliberately
    does not try to reuse a socket for a second call.
    """
    started = time.monotonic()
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(timeout)
    try:
        conn.connect(sock_path)
        request = json.dumps({"id": "stress", "method": method, "params": params or {}})
        conn.sendall(request.encode() + b"\n")
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = conn.recv(65536)
            if not chunk:
                break
            buf += chunk
    except OSError as err:
        return ApiResult(False, (time.monotonic() - started) * 1000, error=str(err))
    finally:
        conn.close()
    elapsed = (time.monotonic() - started) * 1000
    try:
        return ApiResult(True, elapsed, json.loads(buf.decode(errors="replace") or "{}"))
    except json.JSONDecodeError:
        return ApiResult(False, elapsed, error="unparseable response")


def api_flood(sock_path: str, method: str, params: dict | None, *, concurrency: int, rounds: int = 1) -> dict:
    """`concurrency` connections at once, `rounds` times, all reported.

    Connections are opened from threads rather than sequentially because the finding
    under test (B5) is about *simultaneous* admission: a sequential loop of 256
    requests never has more than one connection live and proves nothing.
    """
    results: list[ApiResult] = []
    lock = threading.Lock()

    def one() -> None:
        result = api_request(sock_path, method, params)
        with lock:
            results.append(result)

    for _ in range(rounds):
        # A barrier rather than a start-them-all loop: thread creation is slow enough
        # that the first connection can be answered and closed before the last one is
        # opened, which is exactly the concurrency this is trying to produce.
        started = threading.Barrier(concurrency + 1)

        def gated() -> None:
            started.wait()
            one()

        threads = [threading.Thread(target=gated) for _ in range(concurrency)]
        for thread in threads:
            thread.start()
        started.wait()
        for thread in threads:
            thread.join(timeout=60.0)

    latencies = sorted(r.elapsed_ms for r in results if r.ok)
    return {
        "sent": len(results),
        "ok": sum(1 for r in results if r.ok),
        "failed": sum(1 for r in results if not r.ok),
        "overloaded": sum(1 for r in results if r.ok and r.overloaded),
        "p50_ms": round(_percentile(latencies, 0.50), 1),
        "p99_ms": round(_percentile(latencies, 0.99), 1),
        "max_ms": round(latencies[-1], 1) if latencies else 0.0,
        "errors": sorted({r.error for r in results if r.error})[:5],
    }


def _percentile(sorted_values: list[float], fraction: float) -> float:
    if not sorted_values:
        return 0.0
    index = min(len(sorted_values) - 1, int(fraction * len(sorted_values)))
    return sorted_values[index]


def wait_until(predicate, *, timeout: float = 30.0, interval: float = 0.25) -> bool:
    """Poll rather than sleep. Returns whether the predicate ever held."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return False
