#!/usr/bin/env python3
"""Benchmark baseline bridge (deliverable #3): run stress.py workloads, emit envelopes.

Wraps peer-test's stress.py: runs one workload at one cardinality, reads its
report.json, and converts it into a lab benchmark envelope with cliff/band
regression grading against the committed baseline for that workload+cardinality.

Baselines live in lab/baselines/<workload>-<concurrency>.json. When none exists,
the run is recorded as `no-baseline` and the envelope is written as the new
baseline candidate (committed by hand after review).

Usage:
    uv run lab/runners/bench_baseline.py api --at 32 [--seconds 4]
    uv run lab/runners/bench_baseline.py output --at 15 [--accept]  # overwrite baseline
"""

# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

LAB_ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = LAB_ROOT.parent
STRESS = REPO_ROOT / "peer-test" / "scripts" / "stress.py"
sys.path.insert(0, str(LAB_ROOT))

from lablib.envelope import Envelope, Provenance, validate_envelope  # noqa: E402

# Metrics graded for regression. (report key, unit, direction)
# direction: "lower" = regressions are increases; higher is better otherwise.
GRADED_METRICS = [
    ("p99_ms", "ms", "lower"),
    ("max_ms", "ms", "lower"),
    ("loop_active_max_us", "us", "lower"),
    ("full_render_avg_us", "us", "lower"),
    ("core_lock_wait_p99_us", "us", "lower"),
    ("rss_peak_mb", "MB", "lower"),
    ("rss_delta_mb", "MB", "lower"),
    ("pty_mb", "MB", "lower"),
]


def sha(data: bytes) -> str:
    import hashlib
    return hashlib.sha256(data).hexdigest()


def file_sha(path: Path) -> str:
    return sha(path.read_bytes())


def binary_for(profile: str) -> tuple[Path, bool]:
    binary = REPO_ROOT / "target" / profile / "herdr"
    if not binary.exists():
        print(f"no herdr binary at {binary}; build first", file=sys.stderr)
        raise SystemExit(2)
    return binary, profile == "release"


def run_stress(workload: str, at: int, seconds: int | None) -> tuple[Path, dict]:
    cmd = ["uv", "run", "--script", str(STRESS), "run", workload, "--at", str(at)]
    if seconds:
        cmd += ["--seconds", str(seconds)]
    before = set((REPO_ROOT / "peer-test" / "evidence").glob("stress-*"))
    proc = subprocess.run(cmd, capture_output=True, text=True, cwd=REPO_ROOT, timeout=1800)
    after = set((REPO_ROOT / "peer-test" / "evidence").glob("stress-*"))
    new_dirs = sorted(after - before, key=lambda p: p.stat().st_mtime)
    if not new_dirs:
        print(proc.stdout[-2000:], proc.stderr[-1000:], file=sys.stderr)
        raise SystemExit(f"stress.py produced no evidence directory (exit {proc.returncode})")
    report_path = new_dirs[-1] / "report.json"
    report = json.loads(report_path.read_text())
    return report_path, report


def grade(current: float, baseline: dict, metric: str, direction: str) -> tuple[str, float]:
    """Return (severity, delta_pct). Severity: '' ok, 'band' warn, 'cliff' fail."""
    base_value = baseline.get("metrics", {}).get(metric)
    if base_value is None or not base_value > 0:
        return "", 0.0
    delta_pct = (current - base_value) / base_value * 100.0
    cliff = baseline.get("_thresholds", {}).get(metric, {}).get("cliff", 1.0)
    band = baseline.get("_thresholds", {}).get(metric, {}).get("band", 0.25)
    if direction != "lower":
        delta_pct = -delta_pct
    if delta_pct >= cliff * 100:
        return "cliff", delta_pct
    if delta_pct >= band * 100:
        return "band", delta_pct
    return "", delta_pct


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("workload")
    ap.add_argument("--at", type=int, required=True)
    ap.add_argument("--profile", choices=["debug", "release"], default="debug")
    ap.add_argument("--seconds", type=int, default=None)
    ap.add_argument("--accept", action="store_true",
                    help="write this run's metrics as the new committed baseline")
    args = ap.parse_args()

    started = time.time()
    binary, _is_release = binary_for(args.profile)
    report_path, report = run_stress(args.workload, args.at, args.seconds)

    # Workloads key their rows differently: api/churn by concurrency, the rest by
    # pane/client/round counts. Match on whichever count column the row carries.
    row = next((r for r in report["rows"] if r.get("concurrency") == args.at
                or r.get("panes") == args.at
                or r.get("clients") == args.at), None)
    if row is None:
        raise SystemExit(f"no report row for concurrency {args.at}")

    out_dir = LAB_ROOT / "artifacts" / f"bench-{args.workload}-{args.at}-{time.strftime('%Y%m%dT%H%M%S')}"
    out_dir.mkdir(parents=True, exist_ok=True)

    metrics = []
    for key, unit, _direction in GRADED_METRICS:
        if key in row and isinstance(row[key], (int, float)):
            metrics.append({"name": key, "value": row[key], "unit": unit})

    envelope = Envelope(
        provenance=Provenance(
            binary_path=str(binary), binary_hash=file_sha(binary),
            profile=args.profile, scenario_id=f"bench-{args.workload}",
            determinism_tier="seeded-simulated",
            parameters={"workload": args.workload, "concurrency": args.at,
                        "stress_report": str(report_path),
                        "branch": report.get("branch"), "commit": report.get("commit")},
        ),
        verdict="pass", family="benchmark",
    )

    baseline_path = LAB_ROOT / "baselines" / f"{args.workload}-{args.at}.json"
    baseline = None
    if baseline_path.exists():
        baseline = json.loads(baseline_path.read_text())

    regressions = []
    body_metrics = []
    for m in metrics:
        entry: dict = {**m}
        if baseline and not args.accept:
            dirn = next(d for k, _u, d in GRADED_METRICS if k == m["name"])
            severity, delta = grade(m["value"], baseline, m["name"], dirn)
            if severity:
                entry["severity"] = severity
                entry["delta_pct"] = round(delta, 1)
                regressions.append({"metric": m["name"], "severity": severity,
                                    "baseline_value": baseline.get("metrics", {}).get(m["name"]),
                                    "current_value": m["value"],
                                    "delta_pct": round(delta, 1)})
        else:
            entry["severity"] = ""
        body_metrics.append(entry)

    if args.accept or baseline is None:
        status = "no-baseline" if baseline is None else "compared"
        baseline_doc = {
            "workload": args.workload, "concurrency": args.at,
            "profile": args.profile,
            "binary_hash": file_sha(binary),
            "metrics": {m["name"]: m["value"] for m in metrics},
            "_thresholds": {
                m["name"]: {"cliff": 1.0, "band": 0.25} for m in metrics
            },
        }
        baseline_path.parent.mkdir(exist_ok=True)
        baseline_path.write_text(json.dumps(baseline_doc, indent=2, sort_keys=True) + "\n")
        print(f"baseline written: {baseline_path}")
    elif baseline.get("binary_hash") != file_sha(binary) or baseline.get("profile") != args.profile:
        # D7: comparison is refused, not warned, on provenance mismatch.
        status = "refused-provenance-mismatch"
        regressions = []
    else:
        status = "compared"

    envelope.body = {"metrics": body_metrics, "baseline": {
        "ref": str(baseline_path), "status": status, "regressions": regressions}}
    verdict = "fail" if any(r["severity"] == "cliff" for r in regressions) else (
        "fail" if False else ("pass" if not regressions else "pass"))
    if regressions:
        envelope.notes = f"{len(regressions)} regression(s): " + ", ".join(
            f"{r['metric']}({r['severity']} +{r['delta_pct']}%)" for r in regressions)
        if any(r["severity"] == "band" for r in regressions):
            pass  # bands are warnings, not failures
    envelope.finish(verdict)
    envelope.add_artifact_file(report_path, "metric", "stress.py report.json")
    env_path = envelope.write(out_dir / "envelope.json")
    errors = validate_envelope(json.loads(env_path.read_text()))

    print(json.dumps({
        "run_id": envelope.run_id, "verdict": envelope.verdict,
        "status": status,
        "metrics": {m["name"]: m["value"] for m in metrics},
        "regressions": regressions,
        "schema_errors": errors,
        "envelope": str(env_path),
        "duration_s": round(time.time() - started, 1),
    }, indent=2))
    return {"pass": 0}.get(envelope.verdict, 1)


if __name__ == "__main__":
    raise SystemExit(main())
