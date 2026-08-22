#!/usr/bin/env python3
"""Shared lab library: envelope model + JSON-Schema validation (deliverable #1).

Usage:
    from lablib.envelope import Envelope, load_schema, validate_envelope
"""

from __future__ import annotations

import hashlib
import json
import platform
import socket
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

SCHEMA_PATH = Path(__file__).resolve().parent.parent / "schemas" / "envelope.schema.json"

FAMILIES = ("functional", "characterization", "stress", "benchmark", "fault")
VERDICTS = ("pass", "fail", "error", "refused")
TIERS = ("deterministic", "seeded-simulated", "live-real")
PROFILES = ("debug", "release")


def load_schema() -> dict[str, Any]:
    return json.loads(SCHEMA_PATH.read_text())


def validate_envelope(envelope: dict[str, Any], schema: dict[str, Any] | None = None) -> list[str]:
    """Validate an envelope dict against the schema. Returns list of error strings.

    Tries jsonschema if available; falls back to structural checks that mirror
    the schema's required fields so validation never silently passes.
    """
    schema = schema or load_schema()
    try:
        import jsonschema  # type: ignore

        v = jsonschema.Draft202012Validator(schema)
        return [f"{'/'.join(str(p) for p in e.path)}: {e.message}" for e in v.iter_errors(envelope)]
    except ImportError:
        pass

    errors: list[str] = []
    for key in ("envelope_version", "run_id", "provenance", "verdict", "artifacts"):
        if key not in envelope:
            errors.append(f"missing required field: {key}")
    if "envelope_version" in envelope and envelope["envelope_version"] != 1:
        errors.append(f"envelope_version must be 1, got {envelope['envelope_version']!r}")
    if "verdict" in envelope and envelope["verdict"] not in VERDICTS:
        errors.append(f"verdict must be one of {VERDICTS}, got {envelope['verdict']!r}")
    prov = envelope.get("provenance", {})
    for key in ("binary_path", "binary_hash", "profile", "host", "scenario_id", "determinism_tier", "parameters"):
        if key not in prov:
            errors.append(f"provenance missing required field: {key}")
    if "profile" in prov and prov["profile"] not in PROFILES:
        errors.append(f"profile must be one of {PROFILES}")
    if "determinism_tier" in prov and prov["determinism_tier"] not in TIERS:
        errors.append(f"determinism_tier must be one of {TIERS}")
    if "artifacts" in envelope and not isinstance(envelope["artifacts"], list):
        errors.append("artifacts must be a list")
    for art in envelope.get("artifacts", []):
        for key in ("path", "kind"):
            if key not in art:
                errors.append(f"artifact entry missing required field: {key}")
    if "family" in envelope and envelope["family"] not in FAMILIES:
        errors.append(f"family must be one of {FAMILIES}")
    return errors


def sha256_file(path: str | Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 16), b""):
            h.update(chunk)
    return h.hexdigest()


@dataclass
class Provenance:
    binary_path: str
    binary_hash: str
    profile: str
    scenario_id: str
    determinism_tier: str
    parameters: dict[str, Any] = field(default_factory=dict)
    manifest_ref: str | None = None
    manifest_hash: str | None = None
    seed: int | None = None
    host: dict[str, str] = field(
        default_factory=lambda: {
            "hostname": socket.gethostname(),
            "os": platform.system(),
            "arch": platform.machine(),
        }
    )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "binary_path": self.binary_path,
            "binary_hash": self.binary_hash,
            "profile": self.profile,
            "host": dict(self.host),
            "scenario_id": self.scenario_id,
            "determinism_tier": self.determinism_tier,
            "parameters": dict(self.parameters),
        }
        if self.manifest_ref:
            d["manifest_ref"] = self.manifest_ref
        if self.manifest_hash:
            d["manifest_hash"] = self.manifest_hash
        if self.seed is not None:
            d["seed"] = self.seed
        return d


@dataclass
class Artifact:
    path: str
    kind: str
    note: str | None = None
    bytes: int | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"path": self.path, "kind": self.kind}
        if self.bytes is not None:
            d["bytes"] = self.bytes
        if self.note:
            d["note"] = self.note
        return d


@dataclass
class Envelope:
    provenance: Provenance
    verdict: str
    family: str | None = None
    body: dict[str, Any] = field(default_factory=dict)
    artifacts: list[Artifact] = field(default_factory=list)
    run_id: str = field(default_factory=lambda: f"lab-{time.strftime('%Y%m%dT%H%M%S')}-{uuid.uuid4().hex[:8]}")
    started_at: float = field(default_factory=time.time)
    finished_at: float | None = None
    notes: str | None = None

    def finish(self, verdict: str) -> None:
        self.verdict = verdict
        self.finished_at = time.time()

    def add_artifact_file(self, path: str | Path, kind: str, note: str | None = None) -> Artifact:
        p = Path(path)
        art = Artifact(path=str(p), kind=kind, note=note, bytes=p.stat().st_size)
        self.artifacts.append(art)
        return art

    def to_dict(self) -> dict[str, Any]:
        from datetime import datetime, timezone

        def iso(t: float | None) -> str | None:
            return None if t is None else datetime.fromtimestamp(t, tz=timezone.utc).isoformat()

        d: dict[str, Any] = {
            "envelope_version": 1,
            "run_id": self.run_id,
            "started_at": iso(self.started_at),
            "provenance": self.provenance.to_dict(),
            "verdict": self.verdict,
            "artifacts": [a.to_dict() for a in self.artifacts],
        }
        if self.family:
            d["family"] = self.family
        if self.body:
            d["body"] = self.body
        if self.finished_at is not None:
            d["finished_at"] = iso(self.finished_at)
        if self.notes:
            d["notes"] = self.notes
        return d

    def validate(self) -> list[str]:
        return validate_envelope(self.to_dict())

    def write(self, path: str | Path) -> Path:
        p = Path(path)
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(self.to_dict(), indent=2, sort_keys=True) + "\n")
        return p
