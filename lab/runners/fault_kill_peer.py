#!/usr/bin/env python3
"""Fault-injection runner (deliverable #4): kill a peer server mid-paste.

Scenario (declared in lab/manifests/kill-peer-mid-paste.toml):
  1. Lab: two servers, a peered to b; remote workspace open on a with a
     fresh capture pane focused.
  2. A reader (`stty raw -echo; head -c N > cap`) starts in the peer pane.
  3. The client delivers a bracketed paste in chunks.
  4. One third of the way through, server b is SIGKILLed.
  5. b is rebooted via `lab.py up` and the peer reconnects.

Assertions:
  * recovery: b is running again and the peer view reconnects
  * liveness: after recovery, new input still reaches a fresh peer pane

The interrupted paste itself is explicitly NOT asserted byte-exact: what
happens to in-flight bytes across a SIGKILL is exactly the behavior this
cell characterizes, and its result is recorded as data for review.

Usage:
    uv run lab/runners/fault_kill_peer.py [--profile debug] [--keep-lab]
"""

# /// script
# requires-python = ">=3.11"
# dependencies = []
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

import _common  # noqa: E402
from lablib.envelope import Artifact, Envelope, Provenance, validate_envelope  # noqa: E402
from runners.paste_matrix import (  # noqa: E402
    BRACKET_END,
    BRACKET_START,
    CHUNK,
    LabDriver,
    file_sha,
    payloads,
)

EXIT_OK, EXIT_PRECONDITION = 0, 2


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def open_peer_ws(lab: LabDriver) -> str:
    """Boot the standard topology and return the focused peer pane's id."""
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


def deliver_until(lab: LabDriver, wire: bytes, stop_at: int,
                  kill: callable) -> int:
    """Deliver paste chunks until `stop_at` bytes have been sent, then kill."""
    sent = 0
    killed = False
    for off in range(0, len(wire), CHUNK):
        piece = wire[off:off + CHUNK]
        proc = lab.tmux("send-keys", "-t", "A", "-l",
                        "--", piece.decode("utf-8", "surrogateescape"))
        if proc.returncode != 0:
            raise RuntimeError(f"tmux send-keys failed at {off}")
        sent = off + len(piece)
        if not killed and sent >= stop_at:
            kill()
            killed = True
    return sent


def server_pid(lab: LabDriver, instance: str) -> int:
    status = lab.ok("status")
    return status["instances"][instance]["pid"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", choices=["debug", "release"], default="debug")
    ap.add_argument("--out", default=str(LAB_ROOT / "artifacts"))
    ap.add_argument("--keep-lab", action="store_true")
    args = ap.parse_args()

    started = time.time()
    binary = REPO_ROOT / "target" / args.profile / "herdr"
    if args.profile == "release" or not binary.exists():
        zig = _common.resolve_zig()
        flags = [] if args.profile == "debug" else ["--release"]
        subprocess.run(["cargo", "build", "--locked", *flags], cwd=REPO_ROOT,
                       env={**os.environ, "ZIG": zig}, check=True,
                       stdout=subprocess.DEVNULL)

    out_dir = Path(args.out) / f"fault-kill-peer-{time.strftime('%Y%m%dT%H%M%S')}"
    out_dir.mkdir(parents=True, exist_ok=True)
    capture = out_dir / "interrupted-paste.bin"

    envelope = Envelope(
        provenance=Provenance(
            binary_path=str(binary), binary_hash=file_sha(binary),
            profile=args.profile, scenario_id="fault-kill-peer-mid-paste",
            determinism_tier="seeded-simulated",
            manifest_ref=str(LAB_ROOT / "manifests" / "kill-peer-mid-paste.toml"),
            manifest_hash=file_sha(LAB_ROOT / "manifests" / "kill-peer-mid-paste.toml"),
            parameters={"topology": "a->b", "payload": "large",
                        "fault": {"do": "kill", "target": "server-b",
                                  "when": "one third through the paste"}},
        ),
        verdict="error", family="fault",
    )

    blob = payloads()["large"]
    wire = BRACKET_START + blob + BRACKET_END
    assertions: list[dict] = []
    error = None

    def assert_(name: str, passed: bool, detail: str = "") -> None:
        assertions.append({"name": name,
                           "verdict": "pass" if passed else "fail",
                           "detail": detail})

    lab = LabDriver(f"f{uuid.uuid4().hex[:5]}", binary)
    try:
        lab.up()
        base_pane = open_peer_ws(lab)
        r = lab.ok("cli", "a", "pane", "split", base_pane,
                   "--direction", "down", "--ratio", "0.5", "--focus")
        pane = r["result"]["pane"]["pane_id"]
        lab.ok("cli", "a", "pane", "run", pane,
               f"stty raw -echo; head -c {len(blob)} > {capture}")
        time.sleep(1.5)

        # --- fault: SIGKILL b one third of the way through the paste -------
        b_pid = server_pid(lab, "b")

        def kill_b() -> None:
            os.kill(b_pid, signal.SIGKILL)

        deliver_until(lab, wire, len(wire) // 3, kill_b)
        time.sleep(1.5)

        # The old reader died with b; whatever landed in the capture before the
        # kill is characterized data, not an assertion.
        pre_death_bytes = capture.read_bytes() if capture.exists() else b""
        assert_(f"server-b was killed (pid {b_pid})", True)

        # --- recovery -------------------------------------------------------
        lab.ok("up", "--instances", "a,b", "--peer", "a->b",
               "--no-build", "--bin", binary, timeout=300)
        deadline = time.monotonic() + 30
        running = False
        while time.monotonic() < deadline:
            st = lab.run("status")["payload"]
            inst = st.get("instances", {}).get("b", {})
            if inst.get("running"):
                running = True
                break
            time.sleep(0.5)
        assert_("server-b running again after reboot", running)

        deadline = time.monotonic() + 30
        reconnected = False
        while time.monotonic() < deadline:
            peers = lab.ok("cli", "a", "peer", "list", "--json").get("result", {}).get("peers", [])
            entry = next((p for p in peers if p.get("name") == "b"), None)
            if entry and entry.get("connection") == "connected":
                reconnected = True
                break
            lab.ok("peer", "connect", "a", "b", expect=(0, EXIT_PRECONDITION))
            time.sleep(1.0)
        assert_("peer view reconnected after restart", reconnected)

        # --- liveness after recovery ----------------------------------------
        # herdr restores the peer workspace on its own after the restart; the
        # sidebar already shows the view as open. Re-clicking the picker here
        # toggles rather than opens, so instead verify focus is on the restored
        # peer pane and split a fresh capture pane off it.
        recapture = out_dir / "post-recovery-capture.bin"
        deadline = time.monotonic() + 30
        panes = []
        focused = None
        while time.monotonic() < deadline:
            all_panes = lab.ok("state", "a")["panes"]
            panes = [p for p in all_panes if p.get("peer") == "b"]
            focused = next((p for p in all_panes if p.get("focused")), None)
            if panes and focused and focused.get("peer") == "b":
                break
            time.sleep(1.0)
        if not panes:
            raise RuntimeError("no peer-backed pane after recovery")
        live_pane = focused["pane_id"]
        r = lab.ok("cli", "a", "pane", "split", live_pane,
                   "--direction", "down", "--ratio", "0.5", "--focus")
        live_pane = r["result"]["pane"]["pane_id"]
        lab.ok("cli", "a", "pane", "run", live_pane,
               f"stty raw -echo; head -c 5 > {recapture}")
        time.sleep(1.5)
        probe = b"alive"
        proc = lab.tmux("send-keys", "-t", "A", "-l", "--",
                        "\x1b[200~" + probe.decode() + "\x1b[201~")
        if proc.returncode != 0:
            raise RuntimeError("post-recovery tmux send failed")
        got = b""
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            got = recapture.read_bytes() if recapture.exists() else b""
            if got:
                break
            time.sleep(0.5)
        assert_("paste into peer pane works after recovery", got == probe,
                f"got {got!r}")

        verdict = "fail" if any(a["verdict"] == "fail" for a in assertions) else "pass"
    except Exception as exc:  # noqa: BLE001
        error, verdict = repr(exc), "error"

    interrupted_bytes = capture.read_bytes() if capture.exists() else b""
    envelope.body = {
        "faults": [{"at": "t+~1/3 of paste stream", "do": "kill",
                    "target": "server-b", "params": {"signal": "SIGKILL"}}],
        "recovery_assertions": assertions,
        "characterization": {
            "payload_bytes": len(blob),
            "interrupted_capture_bytes": len(interrupted_bytes),
            "interrupted_capture_hash": sha(interrupted_received(interrupted_bytes, blob)),
            "note": ("in-flight bytes across SIGKILL are characterized data, "
                     "not asserted"),
        },
    }
    envelope.finish(verdict)
    for f in sorted(out_dir.iterdir()):
        if f.is_file():
            envelope.add_artifact_file(f, "state", "fault-run capture")
    env_path = envelope.write(out_dir / "envelope.json")
    schema_errors = validate_envelope(json.loads(env_path.read_text()))

    if not args.keep_lab:
        lab.destroy()

    print(json.dumps({
        "run_id": envelope.run_id, "verdict": envelope.verdict,
        "assertions": {a["name"]: a["verdict"] for a in assertions},
        "interrupted_capture_bytes": len(interrupted_bytes),
        "schema_errors": schema_errors, "error": error,
        "envelope": str(env_path),
        "duration_s": round(time.time() - started, 1),
    }, indent=2))
    return {"pass": 0}.get(envelope.verdict, 1)


def interrupted_received(captured: bytes, blob: bytes) -> bytes:
    """The payload portion of the pre-kill capture (strip herdr's brackets)."""
    if captured.startswith(BRACKET_START):
        return captured[len(BRACKET_START):]
    return captured


if __name__ == "__main__":
    raise SystemExit(main())
