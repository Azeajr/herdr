"""Automatic evidence bundling on failure (backlog: harness ergonomics).

Any runner whose cell fails or errors freezes a lab evidence bundle via
`lab.py evidence` BEFORE destroying the lab, and indexes it in the envelope's
artifact list. Mirrors peer-test conftest's pattern: red cells must arrive
with their own diagnosis material, not depend on --keep-lab.

Usage from a runner:

    from runners.evidence import bundle_on_failure
    ...
    finally:
        bundle_on_failure(lab, envelope, out_dir, verdict, note="...")
"""

from __future__ import annotations

import json
import subprocess
import uuid
from pathlib import Path


def bundle_on_failure(lab, envelope, out_dir: Path, verdict: str,
                      note: str = "") -> str | None:
    """Freeze a lab evidence bundle when `verdict` is fail/error/refused.

    Returns the bundle directory, or None on pass/skip or if freezing failed
    (bundling must never mask the original failure). The bundle is copied into
    out_dir so the run's artifact index stays self-contained.
    """
    if verdict not in ("fail", "error", "refused"):
        return None
    try:
        proc = lab.run("evidence", f"auto-{envelope.run_id}",
                       "--note", note or f"automatic bundle for {verdict} cell")
        payload = proc.get("payload", {})
        src = payload.get("bundle")
        if not src or not Path(src).exists():
            return None
        dest = Path(out_dir) / "evidence-bundle"
        if src != str(dest):
            import shutil

            shutil.copytree(src, dest)
        envelope.add_artifact_file(dest / "timeline.log", "log",
                                   "merged server/client timeline at failure") \
            if (dest / "timeline.log").exists() else None
        for f in sorted(dest.iterdir()):
            if f.suffix == ".json" and f.name.startswith("state-"):
                envelope.add_artifact_file(f, "state", "instance state at failure")
        return str(dest)
    except Exception:  # noqa: BLE001 — bundling is best-effort by contract
        return None
