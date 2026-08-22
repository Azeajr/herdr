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

All four shipped 2026-08-21/22. See the git log for the implementation
commits and `lab/artifacts/` (local) for the verifying envelopes.

## Future work

Backlog of natural next steps, in rough priority order. Each item names
what it is and what it needs; none is designed yet.

### Paste matrix completion

- **middle-click driver.** Primary-selection paste needs a pane program that
  handles mouse reports; zsh does not. Options: run a small helper (e.g. a
  Python reader with mouse mode enabled) as the capture pane's child, or test
  against vim/helix. Blocked on choosing the pane app.

### Performance work (the lab's second consumer)

- **Release-profile baselines.** Current committed baselines are debug.
  Capture release baselines for the same workload/cardinality set so perf
  findings can be stated in release terms (AGENTS.md: prefer release).
- **More cardinalities.** output/memory at 50, api at 256, fanout/churn/input
  workloads — stress.py already supports them; each just needs a baseline
  capture (`bench_baseline.py <workload> --at N`).
- **Baseline refresh ritual.** Baselines rot as herdr changes. Decide the
  policy: re-accept on every upstream rebase? Only on suspicion? Document it
  here once decided.

### Baseline policy (adopted 2026-08-22)

**Layout.** Baselines live at `lab/baselines/<profile>/<workload>-<at>.json`.
A debug number can never be graded against a release baseline or vice versa;
the runner also cross-checks the profile stress.py *measured* against the
profile it was *asked* for and refuses on mismatch (the harness defect where
`--profile release` still measured the debug binary is closed).

**When a baseline may be refreshed** — all of the following must hold:

1. **Legitimate cause**, one of:
   - an upstream rebase or local herdr change that plausibly moves the metric
     (the commit touching a multiplicative path is the trigger);
   - a harness/measurement change (different workload parameters, profiler
     window, machine) that makes old numbers non-comparable;
   - the baseline itself is proven wrong (measured in the wrong profile, on a
     busy machine, with a harness defect now fixed).
2. **The envelope trail proves it.** A refresh is `--accept` on a run whose
   envelope is committed under `lab/artifacts/` with verdict `pass` and
   baseline status recorded; the commit message names the cause (1) and cites
   the run_id.
3. **No red left behind.** A cliff regression may never be cleared by
   refreshing the baseline in the same change that introduced it. First
   investigate; if the regression is genuine herdr behavior, it is either
   fixed or explicitly accepted in a decision note — the baseline then
   refreshes in a *separate* commit referencing that note.

**When baselines may NOT be refreshed:**

- to make a red cell green without a cause from (1);
- on provenance mismatch — a mismatch is `refused`, and the fix is re-running
  with the matching binary/profile, not re-baselining;
- silently: a baseline file change without an envelope + cause in its commit
  message is review-blocking.

**Cadence.** No scheduled re-accept. Debug baselines are working numbers;
release baselines are the authoritative ones for findings and are refreshed
only per the rules above — typically after an upstream rebase that touches
`src/render`, `src/terminal`, `src/pane`, or the client fanout paths.

### Fault injection expansion

- **Network partition cells** via tc/netem in the peer-test Docker boxes:
  partition a↔b during an active paste and idle, verify reconnect and no
  corruption. Needs the boxes.sh topology wired into a lab runner.
- **Slow-peer cells**: netem delay/loss on b; characterize queue behavior and
  the "peer stopped reading" abandonment path in remote.rs.
- **Client-kill cell**: SIGKILL client A mid-paste; verify server-side pane
  state stays sane and a fresh client attaches cleanly.
- **Disk-fill cell**: fill b's state disk during persist; verify graceful
  degradation.

### Harness ergonomics

- **Cron/watchdog mode**: run the matrix + benchmarks on a schedule (or after
  each upstream rebase) and deliver red envelopes to chat. The envelope JSON
  is already LLM-readable; this makes regression review automatic.
- **Evidence bundling on failure**: mirror peer-test's conftest pattern —
  freeze an evidence bundle automatically when any cell fails, instead of
  only keeping artifacts on --keep-lab.
- **Manifest-driven fault choreography**: extend TOML manifests to compose
  faults with scenario phases (the kill-peer runner currently hardcodes its
  sequence in Python).
