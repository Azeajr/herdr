#!/usr/bin/env python3
"""Paste matrix runner (deliverable #2): characterization envelopes for pastes.

Drives peer-test's lab.py exactly the way its pytest fixtures do — as a
subprocess whose JSON output and exit codes are the contract — and emits one
envelope per run conforming to lab/schemas/envelope.schema.json.

v1 coverage: the `bracketed-paste` source, both targets (local pane on `a`,
peer-backed pane reached via a->b), four payloads. Each remote cell is compared
against its local-oracle counterpart by hashing what actually landed in the
pane process's stdin capture file. The other three sources are declared skip
cells until their drivers land.

Usage:
    uv run lab/runners/paste_matrix.py [--profile debug|release] [--out DIR]
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
from lablib.envelope import Envelope, Provenance, validate_envelope  # noqa: E402

EXIT_OK, EXIT_PRECONDITION, EXIT_TIMEOUT = 0, 2, 3

SOURCES = ["osc52", "bracketed-paste", "middle-click", "copy-command"]
TARGETS = ["local:a:p1", "peer:b:w1:p1"]
PAYLOAD_KINDS = ["tiny", "large", "multibyte", "ansi"]

BRACKET_START, BRACKET_END = b"\x1b[200~", b"\x1b[201~"
CHUNK = 4096  # tmux send-keys arg limits; keep well under


def payloads() -> dict[str, bytes]:
    cjk = ("ヘルド貼り付けテスト — emoji 🐛🔥 and combining é marks ").encode()
    ansi = b"\x1b[31mred\x1b[0m \x1b[1;32mgreen\x1b[0m plain "
    return {
        "tiny": b"hello",
        "large": (b"The quick brown fox jumps over the lazy dog. " * 1550)[:65536],
        "multibyte": cjk * 20,
        "ansi": ansi * 400,
    }


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def file_sha(path: Path) -> str:
    return sha(Path(path).read_bytes())


class LabDriver:
    """Subprocess driver mirroring peer-test/tests/conftest.py's Lab class."""

    def __init__(self, name: str, binary: Path):
        self.name, self.binary = name, binary
        env = os.environ.copy()
        for var in _common.INHERITED_HERDR_VARS:
            env.pop(var, None)
        self.env = env

    def run(self, *args, timeout: float = 180.0) -> dict:
        proc = subprocess.run(
            ["uv", "run", "--script", str(PEER_TEST / "scripts" / "lab.py"),
             "--lab", self.name, "--json", *[str(a) for a in args]],
            capture_output=True, text=True, cwd=REPO_ROOT,
            env=self.env, timeout=timeout,
        )
        try:
            payload = json.loads(proc.stdout)
        except json.JSONDecodeError:
            payload = {}
        return {"exit": proc.returncode, "payload": payload}

    def ok(self, *args, expect: tuple[int, ...] = (0,), **kw) -> dict:
        r = self.run(*[str(a) for a in args], **kw)
        if r["exit"] not in expect:
            raise RuntimeError(
                f"lab.py {' '.join(map(str, args))}: exit {r['exit']}\n"
                f"{json.dumps(r['payload'])[:2000]}"
            )
        return r["payload"]

    def up(self) -> None:
        self.ok("up", "--instances", "a,b", "--peer", "a->b",
                "--no-build", "--bin", self.binary, timeout=300)

    def destroy(self) -> None:
        self.run("destroy")

    def tmux(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(["tmux", "-L", f"hl-{self.name}", *[str(a) for a in args]],
                              capture_output=True)

    def ui_open(self, client: str = "A") -> None:
        opened = self.ok("ui", "open", "a", "--client", client)
        if opened.get("gate") == "onboarding":
            self.ok("ui", "onboard", client)


def open_peer_workspace(lab: LabDriver) -> None:
    """Open remote-ws on `a` via the UI path the e2e tests use.

    `up --peer` wires the peer, but on a lab created fresh in one shot the peer
    sometimes isn't in `a`'s list when the client opens; `peer connect` is idempotent
    (it skips `peer add` when the name exists), so re-issuing it closes that race.
    """
    lab.ok("cli", "b", "workspace", "create", "--label", "remote-ws")
    lab.ui_open()
    lab.ok("peer", "connect", "a", "b")
    # Wait for enumeration: sidebar header carries <opened>/<enumerated>.
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if lab.ok("ui", "find", "A", "0/1", expect=(0, 2)).get("matches"):
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
            return
        time.sleep(0.5)
    raise RuntimeError(f"no peer-backed pane after opening workspace: {lab.ok('state', 'a')}")


def focus_workspace(lab: LabDriver, target: str) -> str:
    """Focus the target's workspace and return its base pane id.

    Local cells focus the local workspace; peer cells focus the peer workspace by
    clicking its sidebar row. The base pane is what every cell then splits.
    """
    if target.startswith("local:"):
        lab.ok("cli", "a", "workspace", "focus", "w1")
        return "w1:p1"
    lab.ok("ui", "click", "A", "--text", "\u25be")
    lab.ok("ui", "wait", "A", "--contains", "open workspace on", "--timeout", 15)
    lab.ok("ui", "click", "A", "--text", "remote-ws")
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        panes = [p for p in lab.ok("state", "a")["panes"] if p.get("peer") == "b"]
        if panes:
            return panes[0]["pane_id"]
        time.sleep(0.5)
    raise RuntimeError(f"peer pane missing when focusing: {lab.ok('state', 'a')}")


def fresh_capture_pane(lab: LabDriver, base_pane: str) -> str:
    """Split off a fresh pane, focused, for one cell's capture.

    A cell must never reuse a pane across cells: the stty raw capture leaves the
    pane's shell tty raw, and interleaved `pane run` command text then corrupts
    later cells (the failure that produced the retracted wedge finding). Each
    cell gets its own split, torn down after reading.
    """
    r = lab.ok("cli", "a", "pane", "split", base_pane,
               "--direction", "down", "--ratio", "0.5", "--focus")
    pane_id = r["result"]["pane"]["pane_id"]
    focused = next((p for p in lab.ok("state", "a")["panes"] if p.get("focused")), None)
    if not focused or focused["pane_id"] != pane_id:
        raise RuntimeError(f"split pane {pane_id} did not take focus: {focused}")
    return pane_id


def close_capture_pane(lab: LabDriver, pane_id: str) -> None:
    lab.ok("cli", "a", "pane", "run", pane_id, "exit",
           expect=(0, EXIT_PRECONDITION))
    time.sleep(1.0)


def deliver(lab: LabDriver, wire: bytes) -> None:
    """Feed literal bytes into the herdr client's tmux session, chunked."""
    for off in range(0, len(wire), CHUNK):
        piece = wire[off:off + CHUNK]
        proc = lab.tmux("send-keys", "-t", "A", "-l", "--", piece.decode("utf-8", "surrogateescape"))
        if proc.returncode != 0:
            raise RuntimeError(f"tmux send-keys failed at offset {off}: {proc.stderr!r}")


def run_cell(lab: LabDriver, source: str, target: str, kind: str,
             blob: bytes, capture_file: Path) -> dict:
    """Deliver one paste into a fresh target pane; hash what the pane received."""
    cid = f"{source}/{target}/{kind}"
    base = {
        "cell_id": cid, "source": source, "target": target,
        "payload": {"kind": kind, "bytes": len(blob), "hash": sha(blob)},
    }

    if source != "bracketed-paste":
        # Driver not implemented yet: declared skip cell so the matrix stays visible.
        return {**base, "verdict": "skip"}

    base_pane = focus_workspace(lab, target)
    pane = fresh_capture_pane(lab, base_pane)
    # Capture exactly len(blob) bytes inside the fresh pane. stty raw avoids the
    # kernel's canonical-mode 4096-char line limit (longer lines are discarded);
    # head -c exits on byte count so no EOF keystroke is needed. herdr parses the
    # bracketed-paste markers before they reach the pane, so the pane's PTY carries
    # exactly the payload.
    lab.ok("cli", "a", "pane", "run", pane,
           f"stty raw -echo; head -c {len(blob)} > {capture_file}")

    try:
        time.sleep(1.5)  # let head start consuming stdin
        deliver(lab, BRACKET_START + blob + BRACKET_END)
        deadline = time.monotonic() + 20
        got = b""
        while time.monotonic() < deadline:
            got = capture_file.read_bytes() if capture_file.exists() else b""
            if len(got) >= len(blob):
                break
            time.sleep(0.5)
        got = got[:len(blob)]
    finally:
        close_capture_pane(lab, pane)
    return {
        **base,
        "verdict": "pass" if got == blob else "fail",
        "received_hash": sha(got),
        "received_bytes": len(got),
        "exact": got == blob,
    }


def apply_oracle(cells_by_id: dict[str, dict]) -> None:
    """Peer cells are graded against their local counterparts, not against exactness."""
    for cid, res in list(cells_by_id.items()):
        source, target, kind = cid.split("/", 2)
        if not target.startswith("peer:") or res["verdict"] == "skip":
            continue
        oracle_id = f"{source}/local:a:p1/{kind}"
        oracle = cells_by_id[oracle_id]
        match = res["received_hash"] == oracle["received_hash"]
        res["oracle_cell_id"] = oracle_id
        res["divergence"] = None if match else (
            f"remote {res['received_bytes']}B/{res['received_hash'][:12]} vs local "
            f"{oracle['received_bytes']}B/{oracle['received_hash'][:12]} "
            f"(payload {res['payload']['bytes']}B)")
        res["verdict"] = "pass" if match else "fail"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", choices=["debug", "release"], default="debug")
    ap.add_argument("--out", default=str(LAB_ROOT / "artifacts"))
    ap.add_argument("--keep-lab", action="store_true", help="skip destroy (debugging)")
    args = ap.parse_args()

    started = time.time()
    binary = REPO_ROOT / "target" / args.profile / "herdr"
    if args.profile == "release" or not binary.exists():
        zig = _common.resolve_zig()
        profile_flag = [] if args.profile == "debug" else ["--release"]
        subprocess.run(["cargo", "build", "--locked", *profile_flag], cwd=REPO_ROOT,
                       env={**os.environ, "ZIG": zig}, check=True,
                       stdout=subprocess.DEVNULL)
    if not binary.exists():
        print(f"binary missing after build: {binary}", file=sys.stderr)
        return 2

    out_dir = Path(args.out) / f"paste-matrix-{time.strftime('%Y%m%dT%H%M%S')}"
    (out_dir / "captures").mkdir(parents=True, exist_ok=True)

    envelope = Envelope(
        provenance=Provenance(
            binary_path=str(binary), binary_hash=file_sha(binary),
            profile=args.profile, scenario_id="paste-matrix",
            determinism_tier="live-real",
            manifest_ref=str(LAB_ROOT / "manifests" / "paste-matrix.toml"),
            manifest_hash=file_sha(LAB_ROOT / "manifests" / "paste-matrix.toml"),
            parameters={"topology": "a->b", "client_cols": 120, "client_rows": 40,
                        "implemented_sources": ["bracketed-paste"]},
        ),
        verdict="error", family="characterization",
    )

    lab = LabDriver(f"m{uuid.uuid4().hex[:5]}", binary)
    cells: list[dict] = []
    error = None
    try:
        lab.up()
        open_peer_workspace(lab)
        blobs = payloads()

        # Local reference cells first: they grade the peer cells.
        results: dict[str, dict] = {}
        plan: list[tuple[str, str, str]] = []
        for kind in PAYLOAD_KINDS:
            plan.append(("bracketed-paste", "local:a:p1", kind))
        for kind in PAYLOAD_KINDS:
            plan.append(("bracketed-paste", "peer:b:w1:p1", kind))

        for source, target, kind in plan:
            cap = out_dir / "captures" / f"{cell_slug(source, target, kind)}.bin"
            res = run_cell(lab, source, target, kind, blobs[kind], cap)
            results[res["cell_id"]] = res

        # Declared-but-unimplemented sources become visible skip cells.
        for source in SOURCES:
            if source == "bracketed-paste":
                continue
            for target in TARGETS:
                for kind in PAYLOAD_KINDS:
                    results[f"{source}/{target}/{kind}"] = {
                        "cell_id": f"{source}/{target}/{kind}",
                        "source": source, "target": target,
                        "payload": {"kind": kind, "bytes": len(blobs[kind]),
                                    "hash": sha(blobs[kind])},
                        "verdict": "skip",
                    }

        apply_oracle(results)
        cells = [results[f"{s}/{t}/{k}"] for s in SOURCES for t in TARGETS for k in PAYLOAD_KINDS]
        verdict = "fail" if any(c["verdict"] == "fail" for c in cells) else "pass"
    except Exception as exc:  # noqa: BLE001
        error, verdict = repr(exc), "error"

    envelope.body = {"cells": cells}
    envelope.finish(verdict)
    cap_dir = out_dir / "captures"
    if cap_dir.exists():
        for f in sorted(cap_dir.iterdir()):
            envelope.add_artifact_file(f, "state", "per-cell received-bytes capture")
    env_path = envelope.write(out_dir / "envelope.json")
    envelope.add_artifact_file(env_path, "metric", "this envelope")
    envelope.write(env_path)  # rewrite with the complete artifact index
    schema_errors = validate_envelope(json.loads(env_path.read_text()))

    if not args.keep_lab:
        lab.destroy()

    print(json.dumps({
        "run_id": envelope.run_id, "verdict": envelope.verdict,
        "cells": {c["cell_id"]: c["verdict"] for c in cells},
        "schema_errors": schema_errors, "error": error,
        "envelope": str(env_path),
        "duration_s": round(time.time() - started, 1),
    }, indent=2))
    return {"pass": 0, "fail": 1}.get(envelope.verdict, 2)


def cell_slug(source: str, target: str, kind: str) -> str:
    return f"{source}_{target}_{kind}".replace("/", "_").replace(":", "-")


if __name__ == "__main__":
    raise SystemExit(main())
