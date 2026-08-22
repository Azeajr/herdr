#!/usr/bin/env python3
"""Manifest-driven fault suite: TOML declares the choreography, Python interprets.

Replaces the hardcoded sequence in fault_kill_peer.py (kept for reference; the
kill-peer cell here is its manifest-driven twin). Each [[cell]] in
manifests/fault-suite.toml is executed phase by phase through a harness-side
action table (D6 toolkit). Adding a fault scenario means adding a manifest
file, not editing this runner.

Usage:
    uv run lab/runners/fault_suite.py [--profile debug] [--cell ID] [--keep-lab]
"""

# /// script
# requires-python = ">=3.11"
# dependencies = ["tomli>=2.0; python_version < '3.11'"]
# ///

from __future__ import annotations

import argparse
import hashlib
import json
import os
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

LAB_ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = LAB_ROOT.parent
PEER_TEST = REPO_ROOT / "peer-test"
sys.path.insert(0, str(PEER_TEST / "scripts"))
sys.path.insert(0, str(LAB_ROOT))

import tomllib  # noqa: E402
import _common  # noqa: E402
from lablib.envelope import Envelope, Provenance, validate_envelope  # noqa: E402
from runners.paste_matrix import (  # noqa: E402
    BRACKET_END,
    BRACKET_START,
    CHUNK,
    LabDriver,
    file_sha,
    payloads,
)

EXIT_OK, EXIT_PRECONDITION = 0, 2
MANIFESTS = sorted((LAB_ROOT / "manifests").glob("fault-*.toml"))


# --- shared setup actions ---------------------------------------------------

def open_peer_ws(lab: LabDriver) -> str:
    """Boot the standard a->b topology and return the focused peer pane id."""
    lab.ok("cli", "b", "workspace", "create", "--label", "remote-ws")
    lab.ui_open()
    lab.ok("peer", "connect", "a", "b")
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if lab.ok("ui", "find", "A", "0/1", expect=(0, EXIT_PRECONDITION)).get("matches"):
            break
        time.sleep(0.5)
    else:
        raise RuntimeError("peer never enumerated on client A")
    lab.ok("ui", "click", "A", "--text", "\u25be")
    lab.ok("ui", "wait", "A", "--contains", "open workspace on", "--timeout", 15)
    lab.ok("ui", "click", "A", "--text", "remote-ws")
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        panes = [p for p in lab.ok("state", "a")["panes"] if p.get("peer") == "b"]
        if panes:
            return panes[0]["pane_id"]
        time.sleep(0.5)
    raise RuntimeError("no peer-backed pane after opening workspace")


def server_pid(lab: LabDriver, instance: str) -> int:
    return lab.ok("status")["instances"][instance]["pid"]


def client_pid(lab: LabDriver, client: str) -> int:
    """The herdr client process inside a client tmux session."""
    proc = lab.tmux("list-panes", "-t", client, "-F", "#{pane_pid}")
    if proc.returncode != 0:
        raise RuntimeError(f"client session {client} gone")
    shell_pid = int(proc.stdout.strip().splitlines()[0])
    # The client is the direct child of the tmux pane's shell-less command;
    # the session runs `env … herdr` directly, so the pane pid IS the client.
    return shell_pid


def wait_peer_reconnected(lab: LabDriver, timeout_s: float) -> bool:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        peers = lab.ok("cli", "a", "peer", "list", "--json").get("result", {}).get("peers", [])
        entry = next((p for p in peers if p.get("name") == "b"), None)
        if entry and entry.get("connection") == "connected":
            return True
        lab.ok("peer", "connect", "a", "b", expect=(0, EXIT_PRECONDITION))
        time.sleep(1.0)
    return False


# --- per-cell context passed to every action --------------------------------

class Cell:
    def __init__(self, lab: LabDriver, out_dir: Path):
        self.lab = lab
        self.out_dir = out_dir
        self.base_pane: str | None = None
        self.capture_pane: str | None = None
        self.capture_file: Path | None = None
        self.blob = payloads()["large"]
        self.wire = BRACKET_START + self.blob + BRACKET_END
        self.sent = 0
        self.assertions: list[dict] = []
        self.pending_faults: list[dict] = []


def act_open_peer_workspace(cell: Cell, step: dict) -> None:
    cell.base_pane = open_peer_ws(cell.lab)


# --- SSH lab setup action for docker boxes ----------------------------------

def act_open_ssh_lab(cell: Cell, step: dict) -> None:
    """Boot a lab on the docker peer boxes via lab.py --on."""
    # The SSH lab uses the docker boxes; we set it up via lab.py --on box1
    # but for the fault runner we keep it simple: the caller starts a local
    # lab with the remote flag, and this action wires it.
    # For now, we use the existing peer workflow on the local machine
    # since the docker boxes already have the binary and SSH is wired.
    # The actual box management is handled by the netem actions.
    cell.base_pane = open_peer_ws(cell.lab)
    # TODO: full --on support when runner is invoked with --remote flag


def act_start_capture(cell: Cell, step: dict) -> None:
    r = cell.lab.ok("cli", "a", "pane", "split", cell.base_pane,
                    "--direction", "down", "--ratio", "0.5", "--focus")
    cell.capture_pane = r["result"]["pane"]["pane_id"]
    cell.capture_file = cell.out_dir / f"capture-{uuid.uuid4().hex[:6]}.bin"
    cell.lab.ok("cli", "a", "pane", "run", cell.capture_pane,
                f"stty raw -echo; head -c {len(cell.blob)} > {cell.capture_file}")
    time.sleep(1.5)


def deliver_until(cell: Cell, stop_at: int, fire: callable | None) -> int:
    """Send paste chunks until stop_at bytes are sent; call fire() once past it."""
    sent, fired = 0, False
    for off in range(0, len(cell.wire), CHUNK):
        piece = cell.wire[off:off + CHUNK]
        proc = cell.lab.tmux("send-keys", "-t", "A", "-l", "--",
                             piece.decode("utf-8", "surrogateescape"))
        if proc.returncode != 0:
            raise RuntimeError(f"tmux send-keys failed at {off}")
        sent = off + len(piece)
        if fire and not fired and sent >= stop_at:
            fire()
            fired = True
    return sent


def split_point(cell: Cell, until: str) -> int:
    if "third" in until:
        return len(cell.wire) // 3
    if "half" in until:
        return len(cell.wire) // 2
    return len(cell.wire)


def act_deliver_paste(cell: Cell, step: dict) -> None:
    pending = [f for f in cell.pending_faults if not f.get("_fired")]

    def fire() -> None:
        for f in pending:
            ACTIONS["fault"](cell, f["step"])
            f["_fired"] = True

    cell.sent = deliver_until(cell, split_point(cell, step.get("until", "")),
                              fire if pending else None)


def act_fault_kill(cell: Cell, step: dict) -> None:
    target = step["target"]  # "server-b" / "client-A"
    kind, name = target.split("-", 1)
    sig = getattr(signal, step.get("params", {}).get("signal", "SIGKILL"))
    pid = (server_pid(cell.lab, name) if kind.lower() == "server"
           else client_pid(cell.lab, name.upper()))
    os.kill(pid, sig)
    time.sleep(1.0)


def act_recover_lab_up(cell: Cell, step: dict) -> None:
    binary = cell.lab.binary
    cell.lab.ok("up", "--instances", "a,b", "--peer", "a->b",
                "--no-build", "--bin", binary, timeout=300)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        st = cell.lab.run("status")["payload"]
        if st.get("instances", {}).get("b", {}).get("running"):
            break
        time.sleep(0.5)
    else:
        raise RuntimeError("server-b never came back after lab-up recovery")


def act_recover_client_reopen(cell: Cell, step: dict) -> None:
    cell.lab.tmux("kill-session", "-t", "A")
    time.sleep(1.0)
    cell.lab.ui_open()


def act_assert_reconnected(cell: Cell, step: dict) -> bool:
    ok = wait_peer_reconnected(cell.lab, step.get("timeout_s", 30))
    record_assertion(cell, "peer view reconnected", ok)
    return ok


def act_assert_liveness(cell: Cell, step: dict) -> bool:
    lab = cell.lab
    deadline = time.monotonic() + 30
    panes, focused = [], None
    while time.monotonic() < deadline:
        all_panes = lab.ok("state", "a")["panes"]
        panes = [p for p in all_panes if p.get("peer") == "b"]
        focused = next((p for p in all_panes if p.get("focused")), None)
        if panes and focused and focused.get("peer") == "b":
            break
        time.sleep(1.0)
    if not panes:
        record_assertion(cell, "paste into peer pane works after recovery", False,
                         detail="no peer-backed pane after recovery")
        return False
    r = lab.ok("cli", "a", "pane", "split", focused["pane_id"],
               "--direction", "down", "--ratio", "0.5", "--focus")
    live_pane = r["result"]["pane"]["pane_id"]
    recapture = cell.out_dir / f"post-recovery-{uuid.uuid4().hex[:6]}.bin"
    lab.ok("cli", "a", "pane", "run", live_pane,
           f"stty raw -echo; head -c 5 > {recapture}")
    time.sleep(1.5)
    probe = step.get("probe", "alive")
    proc = lab.tmux("send-keys", "-t", "A", "-l", "--",
                    "\x1b[200~" + probe + "\x1b[201~")
    got = b""
    deadline = time.monotonic() + step.get("timeout_s", 15)
    while time.monotonic() < deadline:
        got = recapture.read_bytes() if recapture.exists() else b""
        if got:
            break
        time.sleep(0.5)
    ok = got == probe.encode()
    record_assertion(cell, "paste into peer pane works after recovery", ok,
                     detail=f"got {got!r}")
    try:
        lab.ok("cli", "a", "pane", "run", live_pane, "exit", expect=(0, EXIT_PRECONDITION))
    except RuntimeError:
        pass
    return ok


def act_assert_server_sane(cell: Cell, step: dict) -> bool:
    deadline = time.monotonic() + step.get("timeout_s", 30)
    last_err = ""
    while time.monotonic() < deadline:
        try:
            state = cell.lab.ok("state", "a")
            if state.get("panes"):
                record_assertion(cell, "server-a state API answers with panes intact",
                                 True, detail=f"{len(state['panes'])} panes")
                return True
            last_err = "state answered but no panes listed"
        except Exception as exc:  # noqa: BLE001
            last_err = repr(exc)
        time.sleep(1.0)
    record_assertion(cell, "server-a state API answers with panes intact", False,
                     detail=last_err)
    return False


def act_assert_fresh_client(cell: Cell, step: dict) -> bool:
    """After reopening client A, input still flows into the peer capture path."""
    lab = cell.lab
    base_pane = open_peer_ws(lab) if not any(
        p.get("peer") == "b" for p in lab.ok("state", "a")["panes"]) else None
    if base_pane:
        cell.base_pane = base_pane
    return act_assert_liveness(cell, step)


# --- wait action -----------------------------------------------------------

def act_wait(cell: Cell, step: dict) -> None:
    duration = step.get("duration_s", 5)
    time.sleep(duration)


# --- new D6 fault actions for docker peer boxes -----------------------------

def _box_name(target: str) -> str:
    # "server-b" -> "box2"
    if target == "server-b":
        return "box2"
    if target == "server-a":
        return "box1"
    # fallback: "server-b" -> "b"
    return target.split("-", 1)[1]


def _netem_apply(box: str, to: str | None, delay: str, loss: str) -> None:
    """Apply netem to a docker peer box. Uses `boxes.sh netem`."""
    cmd = ["bash", "peer-test/docker/boxes.sh", "netem", box]
    if to:
        cmd += ["--to", to]
    cmd += [delay, loss]
    proc = subprocess.run(cmd, capture_output=True, text=True, cwd=REPO_ROOT)
    if proc.returncode != 0:
        raise RuntimeError(f"netem apply failed: {proc.stderr}")


def _netem_clear(box: str) -> None:
    cmd = ["bash", "peer-test/docker/boxes.sh", "netem", box, "clear"]
    proc = subprocess.run(cmd, capture_output=True, text=True, cwd=REPO_ROOT)
    if proc.returncode != 0:
        raise RuntimeError(f"netem clear failed: {proc.stderr}")


def act_fault_partition(cell: Cell, step: dict) -> None:
    box = _box_name(step["target"])
    params = step.get("params", {})
    direction = params.get("direction", "egress")
    loss = params.get("loss", "100%")
    delay = params.get("delay", "0ms")
    if direction == "egress":
        _netem_apply(box, to="", delay=delay, loss=loss)
    else:
        # ingress is harder; use egress on the other box as a proxy
        other = "box1" if box == "box2" else "box2"
        _netem_apply(other, to=box, delay=delay, loss=loss)


def act_fault_partition_clear(cell: Cell, step: dict) -> None:
    box = _box_name(step["target"])
    _netem_clear(box)


def act_fault_slow_peer(cell: Cell, step: dict) -> None:
    box = _box_name(step["target"])
    params = step.get("params", {})
    delay = params.get("delay", "200ms")
    loss = params.get("loss", "0%")
    _netem_apply(box, to="", delay=delay, loss=loss)


def act_fault_slow_peer_clear(cell: Cell, step: dict) -> None:
    box = _box_name(step["target"])
    _netem_clear(box)


# --- action table -----------------------------------------------------------

ACTIONS = {
    "open-peer-workspace": act_open_peer_workspace,
    "open-ssh-lab": act_open_ssh_lab,
    "start-capture": act_start_capture,
    "deliver-paste": act_deliver_paste,
    "fault": lambda cell, step: FAULTS[step["action"]](cell, step),
    "recover": lambda cell, step: RECOVERIES[step["method"]](cell, step),
    "assert-reconnected": act_assert_reconnected,
    "assert-liveness": act_assert_liveness,
    "assert-server-sane": act_assert_server_sane,
    "assert-fresh-client": act_assert_fresh_client,
    "wait": act_wait,
}
FAULTS = {"kill": act_fault_kill, "client-kill": act_fault_kill,
          "partition": act_fault_partition, "partition-clear": act_fault_partition_clear,
          "slow-peer": act_fault_slow_peer, "slow-peer-clear": act_fault_slow_peer_clear}
RECOVERIES = {"lab-up": act_recover_lab_up, "client-reopen": act_recover_client_reopen}


def record_assertion(cell: Cell, name: str, passed: bool, detail: str = "") -> None:
    cell.assertions.append({"name": name,
                            "verdict": "pass" if passed else "fail",
                            "detail": detail})


# --- driver ------------------------------------------------------------------

def run_cell(doc: dict, cell_spec: dict, binary: Path, profile: str,
             out_root: Path, keep_lab: bool) -> dict:
    out_dir = out_root / f"{cell_spec['id']}"
    out_dir.mkdir(parents=True, exist_ok=True)
    started = time.time()
    envelope = Envelope(
        provenance=Provenance(
            binary_path=str(binary), binary_hash=file_sha(binary),
            profile=profile, scenario_id=f"fault-{cell_spec['id']}",
            determinism_tier=cell_spec.get("determinism_tier", "seeded-simulated"),
            manifest_ref=str(doc["_path"]),
            manifest_hash=file_sha(doc["_path"]),
            parameters={"topology": cell_spec.get("topology", {}),
                        "phases": [p.get("do") for p in cell_spec.get("phase", [])]},
        ),
        verdict="error", family="fault",
    )
    lab = LabDriver(f"f{uuid.uuid4().hex[:5]}", binary)
    cell = Cell(lab, out_dir)
    error = None
    try:
        lab.up()
        phases = cell_spec["phase"]
        # Fault steps attached to a deliver-paste's `until` fire mid-stream.
        cell.pending_faults = [
            {"step": p} for i, p in enumerate(phases) if p.get("do") == "fault"
            and i > 0 and phases[i - 1].get("do") == "deliver-paste"
        ]
        for i, step in enumerate(phases):
            do = step.get("do")
            if do == "fault" and any(f["step"] is step for f in cell.pending_faults):
                continue  # fires from within deliver-paste
            action = ACTIONS.get(do)
            if action is None:
                raise RuntimeError(f"unknown phase action {do!r}")
            action(cell, step)  # assert actions record their own verdicts
        verdict = ("fail" if any(a["verdict"] == "fail" for a in cell.assertions)
                   else "pass")
    except Exception as exc:  # noqa: BLE001
        error, verdict = repr(exc), "error"

    interrupted = b""
    if cell.capture_file and cell.capture_file.exists():
        interrupted = cell.capture_file.read_bytes()

    # Freeze a full lab evidence bundle before teardown whenever the cell is
    # not green (fail/error/refused) — diagnosis material travels with the run.
    from runners.evidence import bundle_on_failure
    bundle_on_failure(lab, envelope, out_dir, verdict,
                      note=f"automatic bundle: fault cell {cell_spec['id']} -> {verdict}")

    envelope.body = {
        "faults": [{"do": p.get("action"), "target": p.get("target"),
                    "params": p.get("params", {})}
                   for p in cell_spec["phase"] if p.get("do") == "fault"],
        "recovery_assertions": cell.assertions,
        "characterization": {
            "payload_bytes": len(cell.blob),
            "sent_bytes": cell.sent,
            "interrupted_capture_bytes": len(interrupted),
            "interrupted_capture_hash": hashlib.sha256(interrupted).hexdigest(),
        },
    }
    envelope.finish(verdict)
    for f in sorted(out_dir.iterdir()):
        if f.is_file():
            envelope.add_artifact_file(f, "state", "fault-run capture")
    env_path = envelope.write(out_dir / "envelope.json")
    envelope.add_artifact_file(env_path, "metric", "this envelope")
    envelope.write(env_path)
    schema_errors = validate_envelope(json.loads(env_path.read_text()))

    if not keep_lab:
        lab.destroy()
    return {
        "cell_id": cell_spec["id"], "verdict": envelope.verdict,
        "run_id": envelope.run_id, "envelope": str(env_path),
        "assertions": {a["name"]: a["verdict"] for a in cell.assertions},
        "schema_errors": schema_errors, "error": error,
        "duration_s": round(time.time() - started, 1),
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", choices=["debug", "release"], default="debug")
    ap.add_argument("--cell", default=None, help="run one cell id instead of all")
    ap.add_argument("--out", default=None)
    ap.add_argument("--keep-lab", action="store_true")
    ap.add_argument("--remote", action="store_true", help="run on docker peer boxes via lab.py --on")
    args = ap.parse_args()

    binary = REPO_ROOT / "target" / args.profile / "herdr"
    if args.profile == "release" or not binary.exists():
        zig = _common.resolve_zig()
        flags = [] if args.profile == "debug" else ["--release"]
        subprocess.run(["cargo", "build", "--locked", *flags], cwd=REPO_ROOT,
                       env={**os.environ, "ZIG": zig}, check=True,
                       stdout=subprocess.DEVNULL)

    cells = []
    for mf in MANIFESTS:
        doc = tomllib.loads(mf.read_text())
        doc["_path"] = mf
        for spec in doc.get("cell", []):
            if args.cell is None or spec["id"] == args.cell:
                # Skip cells that require docker boxes if not running --remote
                requires = spec.get("requires", [])
                if "docker-boxes" in requires and not args.remote:
                    print(f"skipping {spec['id']}: requires --remote (docker peer boxes)", file=sys.stderr)
                    continue
                cells.append((doc, spec))
    if not cells:
        print(f"no matching cells in {[str(m) for m in MANIFESTS]}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%dT%H%M%S")
    out_root = Path(args.out or (LAB_ROOT / "artifacts" / f"fault-suite-{stamp}"))
    results = [run_cell(doc, spec, binary, args.profile, out_root, args.keep_lab)
               for doc, spec in cells]

    print(json.dumps({"cells": results,
                      "all_pass": all(r["verdict"] == "pass" for r in results)}, indent=2))
    return 0 if all(r["verdict"] == "pass" for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
