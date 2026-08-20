#!/usr/bin/env -S uv run --script
# /// script
# dependencies = [
#   "click>=8.3.0",
#   "rich>=14.2.0",
# ]
# requires-python = ">=3.12"
# ///

"""
stress — the deterministic load drivers for section 7 of the engineering analysis.

Seven workloads, each at several cardinalities, each answering with the profiler
windows and resource levels its finding is stated in:

    stress.py list
    stress.py run api --at 1,32,256
    stress.py run output --at 1,15,50 --seconds 6
    stress.py run peer --at 1,15 --report /tmp/peer.json

Design notes that are load-bearing:

  * **Lifecycle goes through `lab.py`; load does not.** `lab.py up` and `ui open`
    are subprocesses, the way the pytest suite drives them. Everything inside a
    measured phase talks to the server directly — a `uv run` per request would
    measure uv.

  * **A fresh server per cardinality.** The profiler retains gauge peaks for the
    life of the process on purpose, so 50 panes measured after 15 in the same
    server reports the larger of the two and calls it 50. Each cardinality gets its
    own instance name, hence its own XDG root, hence its own restore state as well.

  * **A real client is attached wherever the terminal is being measured.** A
    measurement of a terminal taken with no client attached is not a measurement of
    that terminal: the same history-capture path reported 116 µs headless and 5-10 ms
    with a client, which was the difference between dismissing a defect and finding it.

  * **Cardinality is tabs, not splits.** Fifty splits in one tab hit the minimum pane
    size and stop being fifty panes; fifty tabs are fifty live PTYs with one visible,
    which is the population the analysis means by "panes".

Exit codes follow lab.py: 0 ok, 2 usage/precondition, 3 timed out.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import click
from rich.console import Console
from rich.table import Table

sys.path.insert(0, str(Path(__file__).resolve().parent))

from _common import (  # noqa: E402  (needs the path insert above)
    EVIDENCE_DIR,
    REPO_ROOT,
    LabError,
    git_ref,
    scrubbed_env,
)
from _stress import (  # noqa: E402
    ProfTail,
    ResourceWatch,
    aggregate,
    api_flood,
    api_request,
    metric,
    wait_until,
)

console = Console(stderr=True)

LAB_SCRIPT = Path(__file__).resolve().parent / "lab.py"
EXIT_OK = 0
EXIT_PRECONDITION = 2
EXIT_TIMEOUT = 3

#: Wide enough that a fifteen-pane sidebar and a terminal both fit, and fixed so a
#: geometry change is a workload's own choice rather than the terminal's.
CLIENT_COLS = 120
CLIENT_ROWS = 40

#: `SESSION_SAVE_DEBOUNCE` in `src/app/mod.rs`. Mirrored rather than derived because
#: the alternative is parsing Rust; a workload that waits less than this measures a
#: save that never happened.
SESSION_SAVE_DEBOUNCE = 5.0


# ---------------------------------------------------------------------------
# the lab, from outside
# ---------------------------------------------------------------------------


@dataclass
class Instance:
    """One live server: where its socket, log and process are."""

    name: str
    sock: str
    log: str
    config: str
    state: str
    pid: int

    @property
    def tail(self) -> ProfTail:
        return ProfTail(self.log)


@dataclass
class Runner:
    """Drives one lab: `lab.py` for lifecycle, the binary and socket for everything else."""

    lab: str
    binary: Path
    keep: bool = False
    clients: list[str] = field(default_factory=list)

    # --- lab.py -----------------------------------------------------------

    def lab_run(self, *args, timeout: float = 300.0, expect: tuple[int, ...] = (EXIT_OK,)) -> dict:
        argv = ["uv", "run", "--script", str(LAB_SCRIPT), "--lab", self.lab, "--json", *[str(a) for a in args]]
        proc = subprocess.run(argv, capture_output=True, text=True, cwd=REPO_ROOT, env=harness_env(), timeout=timeout)
        try:
            payload = json.loads(proc.stdout)
        except json.JSONDecodeError:
            payload = {}
        if proc.returncode not in expect:
            raise LabError(
                f"lab.py {' '.join(str(a) for a in args)} exited {proc.returncode}",
                stdout=proc.stdout[:2000],
                stderr=proc.stderr[-2000:],
            )
        return payload

    def up(self, name: str, **env: str) -> Instance:
        """Boot one instance with the profiler on, leaving any others alone."""
        args = ["up", "--instances", name, "--bin", str(self.binary)]
        # The general-purpose lab enables hitbox snapshots for click-driving.
        # Stress workloads never click controls, and production does not set
        # this debug-only variable; leaving it on charges frame time for cloning
        # sidebar controls that the workload neither uses nor intends to measure.
        profiler_env = {
            "HERDR_RENDER_PROF": "1",
            "HERDR_HITBOX_DUMP": "",
            **env,
        }
        for key, value in profiler_env.items():
            args += ["--env", f"{key}={value}"]
        payload = self.lab_run(*args)
        entry = payload["instances"][name]
        return Instance(
            name=name,
            sock=entry["sock"],
            log=entry["log"],
            config=entry["config"],
            state=entry["state"],
            pid=int(entry["pid"]),
        )

    def peer(self, source: str, target: str) -> dict:
        return self.lab_run("peer", "connect", source, target, expect=(EXIT_OK, EXIT_PRECONDITION))

    def ui_open(self, instance: Instance, client: str, cols: int = CLIENT_COLS, rows: int = CLIENT_ROWS) -> dict:
        opened = self.lab_run("ui", "open", instance.name, "--client", client, "--cols", cols, "--rows", rows)
        if opened.get("gate") == "onboarding":
            self.lab_run("ui", "onboard", client)
        self.clients.append(client)
        return opened

    def ui_close(self, client: str) -> None:
        self.lab_run("ui", "close", client, expect=(EXIT_OK, EXIT_PRECONDITION))
        if client in self.clients:
            self.clients.remove(client)

    def client_pid(self, client: str) -> int | None:
        """The tmux pane pid of a client, for stopping it mid-phase.

        A client that stops reading its socket is the slow reader the fanout workload
        needs, and SIGSTOP is the only way to produce one that is otherwise a real,
        fully attached client.
        """
        proc = subprocess.run(
            ["tmux", "-L", f"hl-{self.lab}", "list-panes", "-t", client, "-F", "#{pane_pid}"],
            capture_output=True,
            text=True,
        )
        first = proc.stdout.split()
        return int(first[0]) if first else None

    def tmux_send(self, client: str, text: str) -> None:
        """Type literal text into a client, without a `uv run` per keystroke.

        `lab.py ui text` is the documented way and costs ~200 ms of interpreter
        startup per call, which is slower than the thing being stressed. This is the
        same `send-keys -l` that command issues. `-l` and never `-H`: hex mode splits
        escape sequences and the input never arrives.
        """
        proc = subprocess.run(
            ["tmux", "-L", f"hl-{self.lab}", "send-keys", "-t", client, "-l", "--", text],
            capture_output=True,
            text=True,
        )
        # Never silent: a send that fails looks exactly like a queue that never filled,
        # which is the conclusion this workload exists to avoid drawing by accident.
        if proc.returncode != 0:
            raise LabError(
                f"tmux send-keys to {client} failed: {proc.stderr.strip()}",
                lab=self.lab,
                client=client,
            )

    def tmux_paste(self, client: str, text: str) -> None:
        """Paste into a client the way a terminal does it: bracketed, in one burst.

        The distinction matters for what a burst costs. `send-keys -l` is typing —
        bytes with no framing — while a paste arrives wrapped in `ESC[200~`/`ESC[201~`,
        which is the form a real user's paste takes.
        """
        buffer = f"stress-{client}"
        loaded = subprocess.run(
            ["tmux", "-L", f"hl-{self.lab}", "load-buffer", "-b", buffer, "-"],
            input=text,
            capture_output=True,
            text=True,
        )
        pasted = subprocess.run(
            ["tmux", "-L", f"hl-{self.lab}", "paste-buffer", "-p", "-b", buffer, "-t", client],
            capture_output=True,
            text=True,
        )
        if loaded.returncode != 0 or pasted.returncode != 0:
            raise LabError(
                f"tmux paste to {client} failed: {(loaded.stderr + pasted.stderr).strip()}",
                lab=self.lab,
                client=client,
            )

    def destroy(self) -> None:
        if self.keep:
            console.print(f"[yellow]lab {self.lab} kept[/]")
            return
        self.lab_run("destroy", expect=(EXIT_OK, EXIT_PRECONDITION))

    # --- the server, directly ---------------------------------------------

    def env_for(self, instance: Instance) -> dict[str, str]:
        return scrubbed_env(Path(instance.config), Path(instance.state))

    def herdr(self, instance: Instance, *args: str, timeout: float = 30.0) -> tuple[int, str]:
        proc = subprocess.run(
            [str(self.binary), *args],
            env=self.env_for(instance),
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=instance.config,
        )
        return proc.returncode, proc.stdout

    def herdr_json(self, instance: Instance, *args: str) -> dict:
        _, stdout = self.herdr(instance, *args)
        try:
            return json.loads(stdout)
        except json.JSONDecodeError:
            return {}

    def api(self, instance: Instance, method: str, params: dict | None = None) -> dict:
        return api_request(instance.sock, method, params).payload

    def stop(self, instance: Instance) -> None:
        self.herdr(instance, "server", "stop")
        wait_until(lambda: not Path(instance.sock).is_socket(), timeout=15.0)

    def run_command(self, instance: Instance, pane_id: str, command: str) -> dict:
        """Type a command into a pane and press Enter.

        There is no `pane.run` on the API — the CLI's `pane run` is sugar for exactly
        this, and going through the socket keeps the drive loop off `uv run`.
        """
        return self.api(instance, "pane.send_text", {"pane_id": pane_id, "text": command + "\n"})

    # --- session shapes ---------------------------------------------------

    def populate(self, instance: Instance, panes: int) -> list[str]:
        """A workspace with `panes` live PTYs, one per tab, and only the first focused."""
        created = self.herdr_json(instance, "workspace", "create", "--label", "stress", "--focus")
        first = created.get("result", {}).get("root_pane", {}).get("pane_id")
        ids = [first] if first else []
        for _ in range(panes - 1):
            tab = self.herdr_json(instance, "tab", "create", "--no-focus")
            pane_id = tab.get("result", {}).get("root_pane", {}).get("pane_id")
            if pane_id:
                ids.append(pane_id)
        return ids

    def fill(self, instance: Instance, pane_ids: list[str], lines: int) -> None:
        """Give each pane real scrollback, then wait until it has actually arrived."""
        for pane_id in pane_ids:
            self.run_command(instance, pane_id, f"seq 1 {lines}")
        for pane_id in pane_ids:
            wait_until(
                lambda pane_id=pane_id: str(lines) in json.dumps(
                    self.api(instance, "pane.read", {"pane_id": pane_id, "source": "visible"})
                ),
                timeout=60.0,
            )


def harness_env() -> dict[str, str]:
    env = os.environ.copy()
    for name in ("HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH", "HERDR_STARTUP_CWD", "HERDR_ENV"):
        env.pop(name, None)
    return env


# ---------------------------------------------------------------------------
# workloads
# ---------------------------------------------------------------------------

WORKLOADS: dict[str, dict] = {}


def workload(name: str, *, default_at: str, needs_tmux: bool, summary: str):
    def register(func):
        WORKLOADS[name] = {
            "run": func,
            "default_at": default_at,
            "needs_tmux": needs_tmux,
            "summary": summary,
        }
        return func

    return register


def loop_row(report: dict) -> dict:
    """The four loop numbers every workload reports, so rows compare across workloads."""
    return {
        "loop_active_avg_us": metric(report, "histograms", "loop.active", "avg_us"),
        "loop_active_p99_us": metric(report, "histograms", "loop.active", "p99_worst_us"),
        "loop_active_max_us": metric(report, "histograms", "loop.active", "max_us"),
        "loop_ticks": metric(report, "counters", "loop.tick", ""),
    }


@workload(
    "output",
    default_at="1,15,50",
    needs_tmux=True,
    summary="Local high output across N panes with a real client attached",
)
def workload_output(run: Runner, at: int, seconds: float, opts: dict) -> dict:
    encoding = opts.get("encoding", "semantic")
    env = {"HERDR_RENDER_ENCODING": "ansi"} if encoding == "ansi" else {}
    instance = run.up(f"o{at}", **env)
    scrollback = opts.get("scrollback")
    if scrollback is not None:
        write_config(instance, f"[advanced]\nscrollback_limit_bytes = {int(scrollback)}\n")
        run.herdr(instance, "server", "reload-config")
    pane_ids = run.populate(instance, at)
    run.ui_open(instance, f"O{at}")

    tail = instance.tail.mark()
    with ResourceWatch(instance.pid) as watch:
        for pane_id in pane_ids:
            run.run_command(instance, pane_id, "seq 1 200000")
        time.sleep(seconds)
    report = aggregate(tail.wait_for_windows(int(seconds)))

    run.ui_close(f"O{at}")
    run.stop(instance)
    return {
        "panes": at,
        "encoding": encoding,
        "scrollback_bytes": scrollback or "default",
        "pty_mb": round(metric(report, "counters", "pty.bytes", "") / 1_048_576, 1),
        **loop_row(report),
        "render_attempts": metric(report, "counters", "render.attempt", ""),
        "full_render_avg_us": metric(report, "durations", "full_render.total", "avg_us"),
        "render_virtual_avg_us": metric(
            report, "durations", "full_render.render_virtual", "avg_us"
        ),
        "core_lock_wait_p99_us": metric(
            report, "histograms", "pty.core_lock.wait", "p99_worst_us"
        ),
        "core_lock_hold_avg_us": metric(
            report, "histograms", "pty.core_lock.hold", "avg_us"
        ),
        "detection_probe_avg_us": metric(
            report, "histograms", "detection.process_probe", "avg_us"
        ),
        "detection_screen_avg_us": metric(
            report, "histograms", "detection.screen_read", "avg_us"
        ),
        "rss_peak_mb": round(watch.peak.rss_kb / 1024),
        "threads_peak": watch.peak.threads,
        "resources": watch.as_dict(),
        "prof": report,
    }


@workload(
    "memory",
    default_at="1,15,50",
    needs_tmux=False,
    summary="Idle PTYs at N panes, before scrollback has been populated",
)
def workload_memory(run: Runner, at: int, seconds: float, opts: dict) -> dict:
    instance = run.up(f"m{at}")
    pane_ids = run.populate(instance, at)
    with ResourceWatch(instance.pid) as watch:
        time.sleep(max(seconds, 1.0))
    run.stop(instance)
    return {
        "panes": at,
        "created": len(pane_ids),
        "rss_peak_mb": round(watch.peak.rss_kb / 1024),
        "rss_delta_mb": round(watch.as_dict()["delta"]["rss_kb"] / 1024, 1),
        "threads_peak": watch.peak.threads,
        "resources": watch.as_dict(),
    }


@workload(
    "detection",
    default_at="1,15,50",
    needs_tmux=True,
    summary="Agent process probing and screen classification across N panes",
)
def workload_detection(run: Runner, at: int, seconds: float, opts: dict) -> dict:
    instance = run.up(f"d{at}")
    pane_ids = run.populate(instance, at)
    run.ui_open(instance, f"D{at}")

    tail = instance.tail.mark()
    command_seconds = max(int(seconds) + 3, 6)
    with ResourceWatch(instance.pid) as watch:
        # `exec -a` gives the foreground job a real agent-shaped argv without
        # starting an external service. Detection still performs its normal
        # process walk and screen read against each live PTY.
        for pane_id in pane_ids:
            run.run_command(
                instance,
                pane_id,
                f"bash -c 'exec -a codex sleep {command_seconds}'",
            )
        time.sleep(seconds)
    report = aggregate(tail.wait_for_windows(max(2, int(seconds))))

    run.ui_close(f"D{at}")
    run.stop(instance)
    return {
        "panes": at,
        "process_probe_count": metric(
            report, "histograms", "detection.process_probe", "count"
        ),
        "process_probe_avg_us": metric(
            report, "histograms", "detection.process_probe", "avg_us"
        ),
        "process_probe_p99_us": metric(
            report, "histograms", "detection.process_probe", "p99_worst_us"
        ),
        "screen_read_count": metric(
            report, "histograms", "detection.screen_read", "count"
        ),
        "screen_read_avg_us": metric(
            report, "histograms", "detection.screen_read", "avg_us"
        ),
        "classify_avg_us": metric(report, "histograms", "detection.classify", "avg_us"),
        "pane_info_avg_us": metric(report, "histograms", "api.pane_info", "avg_us"),
        "pane_info_p99_us": metric(
            report, "histograms", "api.pane_info", "p99_worst_us"
        ),
        "foreground_cwd_avg_us": metric(
            report, "histograms", "api.pane_info.foreground_cwd", "avg_us"
        ),
        "rss_peak_mb": round(watch.peak.rss_kb / 1024),
        "resources": watch.as_dict(),
        "prof": report,
    }


@workload(
    "api",
    default_at="1,32,256",
    needs_tmux=False,
    summary="Concurrent API connections against the admission cap",
)
def workload_api(run: Runner, at: int, seconds: float, opts: dict) -> dict:
    instance = run.up(f"api{at}")
    run.populate(instance, 1)

    tail = instance.tail.mark()
    with ResourceWatch(instance.pid, interval=0.05) as watch:
        flood = api_flood(instance.sock, "pane.list", {}, concurrency=at, rounds=3)
    report = aggregate(tail.wait_for_windows(2))

    run.stop(instance)
    return {
        "concurrency": at,
        "sent": flood["sent"],
        "overloaded": flood["overloaded"],
        "failed": flood["failed"],
        "p99_ms": flood["p99_ms"],
        "max_ms": flood["max_ms"],
        "api_depth_max": metric(report, "gauges", "queue.api.depth", "max"),
        **loop_row(report),
        "threads_peak": watch.peak.threads,
        "threads_delta": watch.as_dict()["delta"]["threads"],
        "fds_delta": watch.as_dict()["delta"]["fds"],
        "resources": watch.as_dict(),
        "flood": flood,
        "prof": report,
    }


@workload(
    "persist",
    default_at="1,15,50",
    needs_tmux=True,
    summary="Session save across N panes holding scrollback, history persistence on",
)
def workload_persist(run: Runner, at: int, seconds: float, opts: dict) -> dict:
    instance = run.up(f"p{at}")
    write_config(instance, "[experimental]\npane_history = true\n")
    run.herdr(instance, "server", "reload-config")

    pane_ids = run.populate(instance, at)
    run.ui_open(instance, f"P{at}")
    run.fill(instance, pane_ids, 20_000)

    tail = instance.tail.mark()
    cycles = max(2, int(seconds) // 3)
    with ResourceWatch(instance.pid) as watch:
        # One structural change, then quiet. Both halves are necessary: a rename
        # schedules nothing at all, and `SESSION_SAVE_DEBOUNCE` is five seconds and is
        # re-armed by every further change — so a driver that churns continuously for
        # six seconds produces exactly zero saves and reports the feature as free.
        for _ in range(cycles):
            created = run.api(instance, "tab.create", {"focus": False})
            tab_id = created.get("result", {}).get("tab", {}).get("tab_id")
            time.sleep(SESSION_SAVE_DEBOUNCE + 1.0)
            if tab_id:
                run.api(instance, "tab.close", {"tab_id": tab_id})
            time.sleep(SESSION_SAVE_DEBOUNCE + 1.0)
    report = aggregate(tail.read())

    run.ui_close(f"P{at}")
    run.stop(instance)
    return {
        "panes": at,
        "saves_driven": cycles * 2,
        "capture_count": metric(report, "histograms", "persist.capture_history", "count"),
        "capture_avg_us": metric(report, "histograms", "persist.capture_history", "avg_us"),
        "capture_max_us": metric(report, "histograms", "persist.capture_history", "max_us"),
        "resolve_avg_us": metric(report, "histograms", "persist.resolve_history", "avg_us"),
        "resolve_max_us": metric(report, "histograms", "persist.resolve_history", "max_us"),
        **loop_row(report),
        "rss_peak_mb": round(watch.peak.rss_kb / 1024),
        "resources": watch.as_dict(),
        "prof": report,
    }


@workload(
    "fanout",
    default_at="1,5,15",
    needs_tmux=True,
    summary="N attached clients at distinct geometries, one of them stopped mid-phase",
)
def workload_fanout(run: Runner, at: int, seconds: float, opts: dict) -> dict:
    instance = run.up(f"f{at}")
    pane_ids = run.populate(instance, 1)

    # Distinct geometry per client by default: identical sizes may let one prepared
    # frame serve every client, and separating the two is the whole of hypothesis 6.
    # `--opt geom=same` is the other half of that comparison.
    distinct = opts.get("geom", "distinct") != "same"
    names = []
    for index in range(at):
        name = f"F{at}_{index}"
        offset = index if distinct else 0
        run.ui_open(instance, name, cols=CLIENT_COLS - offset * 4, rows=CLIENT_ROWS - offset)
        names.append(name)

    # `--opt slow=0` is the control run: with it, the only difference between
    # cardinalities is the number of geometries, which is what hypothesis 6 asks about.
    slow = opts.get("slow", "1") != "0"
    stopped = run.client_pid(names[-1]) if at > 1 and slow else None
    tail = instance.tail.mark()
    with ResourceWatch(instance.pid) as watch:
        if stopped:
            os.kill(stopped, 19)  # SIGSTOP: a real client that has stopped reading
        run.run_command(instance, pane_ids[0], "seq 1 200000")
        time.sleep(seconds)
        if stopped:
            os.kill(stopped, 18)  # SIGCONT
    report = aggregate(tail.wait_for_windows(int(seconds)))

    for name in names:
        run.ui_close(name)
    run.stop(instance)
    return {
        "clients": at,
        "geometry": "distinct" if distinct else "same",
        "slow_reader": bool(stopped),
        "control_items_max": metric(report, "gauges", "queue.client_control.items", "max"),
        "control_kb_max": metric(report, "gauges", "queue.client_control.bytes", "max") // 1024,
        "full_render_count": metric(report, "durations", "full_render.total", "count"),
        "full_render_avg_us": metric(report, "durations", "full_render.total", "avg_us"),
        "full_render_max_us": metric(report, "durations", "full_render.total", "max_us"),
        "frames_prepared": metric(report, "counters", "prepare_frame.semantic.changed", ""),
        "client_frame_mb": round(metric(report, "counters", "full_render.client_bytes", "") / 1_048_576, 2),
        "serialize_avg_us": metric(report, "histograms", "full_render.serialize", "avg_us"),
        **loop_row(report),
        "rss_peak_mb": round(watch.peak.rss_kb / 1024),
        "resources": watch.as_dict(),
        "prof": report,
    }


@workload(
    "peer",
    default_at="1,15",
    needs_tmux=True,
    summary="N peer-backed views on one server while the far side produces output",
)
def workload_peer(run: Runner, at: int, seconds: float, opts: dict) -> dict:
    far = run.up(f"pf{at}")
    near = run.up(f"pn{at}")
    run.peer(near.name, far.name)

    far_panes = run.populate(far, at)
    far_id = read_instance_id(far)
    run.ui_open(near, f"PN{at}")

    # `peer open` rather than `peer.terminal.open`: the second opens a controller with
    # no pane around it, which never reaches the render path this workload measures.
    opened = 0
    for pane_id in far_panes:
        response = run.herdr_json(
            near, "peer", "open", f"{far_id}:{pane_id}", "--peer", far.name, "--no-focus"
        )
        opened += 1 if response.get("ok") or "result" in response else 0

    near_tail = near.tail.mark()
    far_tail = far.tail.mark()
    with ResourceWatch(near.pid) as watch:
        for pane_id in far_panes:
            run.run_command(far, pane_id, "seq 1 50000")
        time.sleep(seconds)
    near_report = aggregate(near_tail.wait_for_windows(int(seconds)))
    far_report = aggregate(far_tail.wait_for_windows(int(seconds)))

    run.ui_close(f"PN{at}")
    run.stop(near)
    run.stop(far)
    # Which side owns which metric is not symmetric, and getting it wrong reads as a
    # feature that costs nothing. The writer queue and `remote.write` belong to the
    # side that *opened* the view, because that is the side holding the socket it
    # writes input on. The frames flow the other way, and are paid for by the far
    # server's own render loop.
    return {
        "peer_panes": at,
        "opened": opened,
        "near_loop_avg_us": metric(near_report, "histograms", "loop.active", "avg_us"),
        "near_loop_max_us": metric(near_report, "histograms", "loop.active", "max_us"),
        "far_loop_avg_us": metric(far_report, "histograms", "loop.active", "avg_us"),
        "far_loop_max_us": metric(far_report, "histograms", "loop.active", "max_us"),
        "far_peer_frame_mb": round(metric(far_report, "counters", "full_render.peer_bytes", "") / 1_048_576, 2),
        "far_frames_sent": metric(far_report, "counters", "full_render.sent", ""),
        "far_frames_skipped": metric(far_report, "counters", "full_render.skip_identical", ""),
        "far_serialize_avg_us": metric(far_report, "histograms", "full_render.serialize", "avg_us"),
        "far_pty_mb": round(metric(far_report, "counters", "pty.bytes", "") / 1_048_576, 1),
        "writer_items_max": metric(near_report, "gauges", "queue.remote_writer.items", "max"),
        "writer_kb_max": metric(near_report, "gauges", "queue.remote_writer.bytes", "max") // 1024,
        "remote_write_avg_us": metric(near_report, "histograms", "remote.write", "avg_us"),
        "remote_write_max_us": metric(near_report, "histograms", "remote.write", "max_us"),
        "frame_store_wait_p99_us": metric(
            near_report, "histograms", "remote.frame.store_lock_wait", "p99_worst_us"
        ),
        "frame_store_hold_avg_us": metric(
            near_report, "histograms", "remote.frame.store_lock_hold", "avg_us"
        ),
        "frame_render_wait_p99_us": metric(
            near_report, "histograms", "remote.frame.render_lock_wait", "p99_worst_us"
        ),
        "frame_render_hold_avg_us": metric(
            near_report, "histograms", "remote.frame.render_lock_hold", "avg_us"
        ),
        "rss_peak_mb": round(watch.peak.rss_kb / 1024),
        "resources": watch.as_dict(),
        "prof": near_report,
        "prof_far": far_report,
    }


@workload(
    "churn",
    default_at="5,20",
    needs_tmux=False,
    summary="N disconnect/reconnect rounds against a peer with an open view",
)
def workload_churn(run: Runner, at: int, seconds: float, opts: dict) -> dict:
    far = run.up("cf")
    near = run.up("cn")
    run.peer(near.name, far.name)
    far_panes = run.populate(far, 1)
    far_id = read_instance_id(far)
    run.herdr_json(near, "peer", "open", f"{far_id}:{far_panes[0]}", "--peer", far.name, "--no-focus")

    tail = near.tail.mark()
    recoveries = []
    with ResourceWatch(near.pid) as watch:
        for _ in range(at):
            run.stop(far)
            far = run.up("cf")
            started = time.monotonic()
            reconnected = wait_until(
                lambda: any(
                    peer.get("connection") == "connected"
                    for peer in peers_of(run, near)
                    if peer.get("name") == far.name
                ),
                timeout=60.0,
            )
            recoveries.append(round((time.monotonic() - started) * 1000) if reconnected else -1)
    report = aggregate(tail.wait_for_windows(2))
    cleanups = sum(peer.get("failed_pane_cleanups", 0) for peer in peers_of(run, near))

    run.stop(near)
    run.stop(far)
    resources = watch.as_dict()
    return {
        "rounds": at,
        "recovered": sum(1 for value in recoveries if value >= 0),
        "failed_pane_cleanups": cleanups,
        "recovery_p50_ms": sorted(recoveries)[len(recoveries) // 2] if recoveries else -1,
        "recovery_max_ms": max(recoveries) if recoveries else -1,
        "threads_delta": resources["delta"]["threads"],
        "fds_delta": resources["delta"]["fds"],
        "rss_delta_mb": round(resources["delta"]["rss_kb"] / 1024, 1),
        **loop_row(report),
        "resources": resources,
        "recoveries_ms": recoveries,
        "prof": report,
    }


@workload(
    "input",
    default_at="256,2048",
    needs_tmux=True,
    summary="N input events into a pane whose child has stopped reading its PTY",
)
def workload_input(run: Runner, at: int, seconds: float, opts: dict) -> dict:
    """Both input paths into a stalled pane, because they fail differently.

    The API path awaits queue capacity, so it can only ever be slow. The client path
    is the one that calls `try_write_user_input` and is handed a `Full` — which is the
    path the hypothesis is about. `--opt via=api` measures the other one.
    """
    via = opts.get("via", "ui")  # ui | paste | api
    instance = run.up(f"i{at}")
    pane_ids = run.populate(instance, 2)
    reading, stalled = pane_ids[0], pane_ids[1]
    # `cat` drains its stdin; a sleeping child never does, so input accumulates below
    # the queue instead of being consumed.
    run.run_command(instance, reading, "cat > /dev/null")
    run.run_command(instance, stalled, "sleep 600")
    time.sleep(1.0)

    client = f"I{at}"
    if via in ("ui", "paste"):
        run.ui_open(instance, client)
        # Focus the stalled pane: client keystrokes go to whatever is focused, and the
        # populate order leaves the *first* tab focused.
        run.api(instance, "pane.focus", {"pane_id": stalled})
        # And confirm the client actually delivers, before concluding anything from a
        # quiet queue. A tmux send that exits 0 but reaches nothing is indistinguishable
        # from a queue that never filled, and that mistake has been made here already.
        probe = instance.tail.mark()
        run.tmux_send(client, "#probe")
        if not wait_until(
            lambda: any(w.counters.get("pty.bytes", 0) for w in probe.read()), timeout=15.0
        ):
            raise LabError(
                "client input never reached the pane; the measurement below would be meaningless",
                client=client,
                pane=stalled,
            )

    tail = instance.tail.mark()
    payload = "x" * int(opts.get("bytes", "512"))
    results = {}
    with ResourceWatch(instance.pid) as watch:
        if via in ("ui", "paste"):
            started = time.monotonic()
            send = run.tmux_paste if via == "paste" else run.tmux_send
            for _ in range(at):
                send(client, payload)
            results["client"] = {
                "sent": at,
                "elapsed_ms": round((time.monotonic() - started) * 1000),
            }
        else:
            for label, pane_id in (("reading", reading), ("stalled", stalled)):
                started = time.monotonic()
                accepted = errors = 0
                for _ in range(at):
                    response = run.api(instance, "pane.send_text", {"pane_id": pane_id, "text": payload})
                    accepted += 1 if "result" in response else 0
                    errors += 1 if "error" in response else 0
                results[label] = {
                    "accepted": accepted,
                    "errors": errors,
                    "elapsed_ms": round((time.monotonic() - started) * 1000),
                }
        # How long the server stays unable to answer. A two-second per-call timeout so
        # a wedged loop reports as unresponsive rather than as one slow call.
        recovery_started = time.monotonic()
        responsive = wait_until(
            lambda: api_request(instance.sock, "pane.list", {}, timeout=2.0).ok,
            timeout=180.0,
            interval=0.5,
        )
        recovery_s = round(time.monotonic() - recovery_started, 1)
        time.sleep(1.5)
    report = aggregate(tail.read())

    if via in ("ui", "paste"):
        run.ui_close(client)
    run.stop(instance)
    reads = metric(report, "counters", "pty.read.syscall", "")
    deliveries = metric(report, "counters", "pty.read.delivery", "")
    return {
        "events": at,
        "via": via,
        "payload_bytes": len(payload),
        "elapsed_ms": max(entry["elapsed_ms"] for entry in results.values()),
        "recovery_s": recovery_s if responsive else -1,
        "pty_kb": metric(report, "counters", "pty.bytes", "") // 1024,
        "pty_reads": reads,
        "pty_deliveries": deliveries,
        "pty_input_items": metric(report, "counters", "pty.input.items", ""),
        "pty_writes": metric(report, "counters", "pty.write.syscall", ""),
        "bytes_per_write": metric(report, "counters", "pty.write.bytes", "")
        // max(metric(report, "counters", "pty.write.syscall", ""), 1),
        "bytes_per_delivery": metric(report, "counters", "pty.bytes", "") // max(deliveries, 1),
        "worst_loop_gap_s": report.get("worst_loop_gap_s", 0),
        "route_batches": metric(report, "counters", "input.route.batches", ""),
        "route_events": metric(report, "counters", "input.route.events", ""),
        "route_batch_avg_us": metric(report, "histograms", "input.route.batch", "avg_us"),
        "route_batch_max_us": metric(report, "histograms", "input.route.batch", "max_us"),
        "pty_input_items_max": metric(report, "gauges", "queue.pty_input.items", "max"),
        "pty_input_dropped": metric(report, "counters", "queue.pty_input.rejected", ""),
        **loop_row(report),
        "resources": watch.as_dict(),
        "results": results,
        "prof": report,
    }


# ---------------------------------------------------------------------------
# helpers the workloads share
# ---------------------------------------------------------------------------


def write_config(instance: Instance, text: str) -> None:
    """Append to the instance's own config.toml, creating it if herdr has not yet."""
    path = Path(instance.sock).parent / "config.toml"
    existing = path.read_text() if path.is_file() else ""
    path.write_text(existing + ("\n" if existing and not existing.endswith("\n") else "") + text)


def read_instance_id(instance: Instance) -> str:
    path = Path(instance.sock).parent / "instance-id"
    return path.read_text().strip() if path.is_file() else ""


def peers_of(run: Runner, instance: Instance) -> list[dict]:
    return run.herdr_json(instance, "peer", "list", "--json").get("result", {}).get("peers", [])


# ---------------------------------------------------------------------------
# reporting
# ---------------------------------------------------------------------------

#: Everything a row carries that belongs in the JSON but would make the table
#: unreadable. The table is for the person watching; the JSON is the evidence.
BULKY = ("prof", "prof_far", "resources", "flood", "results", "recoveries_ms")


def render_table(name: str, rows: list[dict]) -> str:
    """A markdown table.

    Not a `rich` one: these rows are fifteen columns wide, and rich fits them to the
    terminal by truncating every header to three characters. A markdown table survives
    a narrow terminal and pastes straight into the plan document, which is where these
    numbers end up anyway.
    """
    columns = [key for key in rows[0] if key not in BULKY]
    lines = ["| " + " | ".join(columns) + " |", "|" + "|".join("---" for _ in columns) + "|"]
    for row in rows:
        lines.append("| " + " | ".join(str(row.get(column, "")) for column in columns) + " |")
    return "\n".join(lines)


def build_profile(binary: Path) -> str:
    """"debug", "release", or the parent directory name for anything else.

    Recorded because it is the difference between a finding and an artefact, and
    nothing else in the report reveals it. Seven phases of this plan were measured
    against a debug build without saying so, which closed one finding that was not
    real and mis-ranked the rest.
    """
    parent = binary.resolve().parent.name
    return parent if parent in ("debug", "release") else parent


def write_report(name: str, rows: list[dict], destination: Path | None, binary: Path) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H-%M-%S")
    branch, commit = git_ref()
    payload = {
        "workload": name,
        "at": stamp,
        "branch": branch,
        "commit": commit,
        "binary": str(binary),
        "profile": build_profile(binary),
        "loadavg": Path("/proc/loadavg").read_text().split()[:3],
        "rows": rows,
    }
    path = destination or (EVIDENCE_DIR / f"stress-{name}-{stamp}" / "report.json")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2))
    return path


# ---------------------------------------------------------------------------
# cli
# ---------------------------------------------------------------------------


@click.group(context_settings={"help_option_names": ["-h", "--help"]})
def cli() -> None:
    """Deterministic load drivers for the analysis's performance harness."""


@cli.command("list")
def list_workloads() -> None:
    """What can be run, and at what cardinalities by default."""
    table = Table(header_style="bold")
    table.add_column("workload")
    table.add_column("default --at")
    table.add_column("needs tmux")
    table.add_column("what it drives")
    for name, spec in WORKLOADS.items():
        table.add_row(name, spec["default_at"], "yes" if spec["needs_tmux"] else "no", spec["summary"])
    Console().print(table)


@cli.command("run")
@click.argument("name")
@click.option("--at", default=None, help="Comma-separated cardinalities (default: the workload's own).")
@click.option("--seconds", default=5.0, help="Length of each measured phase.")
@click.option("--lab", "lab_name", default="stress", help="Lab name (root: /tmp/hl-<name>).")
@click.option(
    "--bin",
    "binary",
    default=None,
    help="herdr binary (default: target/debug/herdr; pass target/release/herdr for real numbers).",
)
@click.option("--keep", is_flag=True, help="Leave the lab up afterwards for inspection.")
@click.option("--report", "report_path", default=None, help="Write the JSON report here instead of evidence/.")
@click.option("--opt", "options", multiple=True, help="KEY=VALUE passed to the workload, e.g. --opt slow=0. Repeatable.")
def run_workload(
    name: str,
    at: str | None,
    seconds: float,
    lab_name: str,
    binary: str | None,
    keep: bool,
    report_path: str | None,
    options: tuple[str, ...],
) -> None:
    """Run one workload at every cardinality and report."""
    spec = WORKLOADS.get(name)
    if spec is None:
        raise click.ClickException(f"no workload '{name}'; try: {', '.join(WORKLOADS)}")
    if spec["needs_tmux"] and not shutil.which("tmux"):
        raise click.ClickException(f"workload '{name}' attaches real clients and needs tmux")

    path = Path(binary).resolve() if binary else REPO_ROOT / "target" / "debug" / "herdr"
    if not path.is_file():
        raise click.ClickException(f"no herdr binary at {path}; build first")

    cardinalities = [int(value) for value in (at or spec["default_at"]).split(",") if value.strip()]
    opts: dict[str, str] = {}
    for pair in options:
        key, sep, value = pair.partition("=")
        if not sep:
            raise click.ClickException(f"bad --opt '{pair}'; use KEY=VALUE")
        opts[key] = value

    run = Runner(lab=lab_name, binary=path, keep=keep)
    rows: list[dict] = []
    try:
        for cardinality in cardinalities:
            console.print(f"[bold]{name}[/] at {cardinality} …")
            rows.append(spec["run"](run, cardinality, seconds, opts))
    finally:
        for client in list(run.clients):
            run.ui_close(client)
        run.destroy()

    destination = write_report(name, rows, Path(report_path) if report_path else None, path)
    print(render_table(name, rows))
    console.print(f"[green]report[/] {destination}  [dim]profile[/] {build_profile(path)}")


def main() -> None:
    try:
        cli(standalone_mode=False)
    except LabError as err:
        print(json.dumps({"ok": False, "error": str(err), **err.details}))
        sys.exit(err.code)
    except click.ClickException as err:
        print(json.dumps({"ok": False, "error": err.format_message()}))
        sys.exit(EXIT_PRECONDITION)
    except subprocess.TimeoutExpired as err:
        print(json.dumps({"ok": False, "error": f"timed out: {err}"}))
        sys.exit(EXIT_TIMEOUT)


if __name__ == "__main__":
    main()
