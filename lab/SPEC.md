# Herdr Lab — Development Laboratory Spec

Status: design locked 2026-08-21. No implementation yet.

## Purpose

A comprehensive test/dev/performance laboratory for herdr: high-fidelity
simulation of real usage — real processes, PTYs, clients, agents, workspaces,
panes, peer networking — with every observable captured as machine-readable
evidence suitable for LLM-driven analysis. A development laboratory, not a CI
suite: the harness outlives any single test and evidence capture is
first-class.

Primary near-term consumers:

1. Performance measurement for further perf work (cardinality-driven
   benchmarks, stored baselines, provenance-checked comparison).
2. Characterization of copy/paste behavior across remote peers, using local
   paste behavior as the reference oracle; known bugs pinned as regression
   cells within the matrix.

## Locked decisions

| # | Decision |
|---|---|
| D1 | Home: this top-level `lab/` directory in the repo. `peer-test/` ports here incrementally when touched, not big-bang. Small JSON summaries committed as baselines; heavy artifacts (screenshots, ANSI captures) stay local under `lab/artifacts/` (gitignored). |
| D2 | Instrumentation: black-box first. Env-gated hooks inside herdr are added one at a time, each justified by a named question that external observation cannot answer (candidate hook #1: input-to-phosphor latency across a peer). Hook output must land in the same artifact schema as external captures. |
| D3 | First scenario family: the paste matrix. Sources: OSC 52, bracketed paste, middle-click, explicit copy command × Targets: local pane, remote pane via A→B peer × Payload sizes: tiny, large, multibyte, ANSI-laden. Local-paste result is the oracle; any remote cell that diverges from its local counterpart is a finding. Known bugs become permanent regression cells. |
| D4 | Result schema: versioned common envelope (provenance, verdict, artifact index) + per-family bodies (`functional`, `characterization`, `stress`, `benchmark`, `fault`). Event streams live inside artifacts as `timeline.jsonl`, not as the top-level contract. See `schemas/envelope.schema.json`. |
| D5 | Scenario declaration: TOML manifests for parametric scenarios and matrices; registered Python classes as the escape hatch for choreography manifests can't express. Both emit identical envelopes. Matrices are data so an LLM can enumerate/propose cells. |
| D6 | Fault injection: harness-side toolkit only — SIGKILL, SIGSTOP/resume, socket close, tc/netem network faults (in the Docker boxes), disk fill — declared as data in the manifest (`[[fault]] at/do/target`). Internal (Herdr-side) injection deferred until a specific recovery question demands it. |
| D7 | Regression policy: each benchmark metric declares `cliff` (hard fail) and `band` (warn/track) in its manifest. Baselines are committed envelope+metrics JSON snapshots. Comparison is REFUSED (not warned) on provenance mismatch: binary hash, profile (debug/release), geometry/cardinality. |
| D8 | Runtime: Python + uv (PEP 723 style), pytest for the functional tier, shared envelope model + JSON-schema validation to catch drift. Rust micro-helpers only if a benchmark demonstrates capture throughput is the bottleneck. |

## Directory plan (when implementation starts)

```
lab/
  SPEC.md            <- this file
  schemas/           <- versioned JSON Schemas (envelope, family bodies)
    envelope.schema.json
  manifests/         <- TOML scenario/matrix declarations
    paste-matrix.toml        (first family)
  lablib/            <- shared Python package: envelope model, runners,
                        capture bus, fault toolkit, artifact store
  baselines/         <- committed baseline snapshots (small JSON)
  artifacts/         <- run output (gitignored)
```

## Envelope contract (summary)

Every run emits one envelope document conforming to
`schemas/envelope.schema.json`:

- `envelope_version` — integer, bumped on breaking change
- `run_id` — unique per execution
- `provenance` — binary path + hash, build profile, host info, timestamp,
  scenario id + manifest ref, geometry/cardinality parameters
- `verdict` — pass / fail / error / refused (refused = e.g. provenance
  mismatch against baseline)
- `body` — family-specific object (see D4 families)
- `artifacts` — index of emitted files: path, kind (log/screen/timeline/
  state/metric), size

Determinism tiers (recorded in provenance): `deterministic`,
`seeded-simulated`, `live-real`. Failures in higher tiers are triaged
differently than regressions in tier 1.

## First deliverables (in order)

1. `schemas/envelope.schema.json` + Python envelope model with validation.
2. Paste-matrix manifest + runner producing envelopes; local-oracle diff view.
3. Benchmark manifests porting stress.py workloads onto cliff/band baselines.
4. Fault toolkit wired into manifests (kill/partition during large paste).
