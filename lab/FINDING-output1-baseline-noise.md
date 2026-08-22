# FINDING: output-1 release baseline for loop_active_max_us is unrepresentative

**Date:** 2026-08-22
**Classification:** harness defect (baseline capture), not a Herdr regression

## What happened

Re-running `bench_baseline.py output --at 1 --profile release` against the
committed release baseline fails on `loop_active_max_us` roughly half the time:

| run | loop_active_max_us | loadavg[0] |
|-----|--------------------|------------|
| baseline capture (T003248) | 2221 | 8.83 |
| rerun A | 7614 | 1.09 |
| rerun B | 5332 | 0.92 |
| rerun C | 2577 | 0.57 |
| rerun D | 5410 | 1.49 |
| rerun E | 2668 | 1.41 |

Same binary (8bbe8741…), same profile, same workload. The baseline value is
the *minimum* of six runs; median is ~4000.

## Root cause

`loop_active_max_us` is a worst-of-N statistic: the single worst event-loop
activation across ~4 profiler windows (~45–50 ticks) at cardinality 1. With
so few samples, one scheduler hiccup sets the number; the distribution across
identical runs spans 2221–7614 us (3.4x). The committed baseline happened to
catch the luckiest run — captured, ironically, at loadavg 8.83 while earlier
stress evidence dirs were still draining, which appears to have suppressed
tail events rather than caused them.

The stable metrics tell the real story: `loop_active_avg_us` spans only
386–598 us (55%) and every other graded metric reproduces within band.

This is exactly the failure mode D7's cliff threshold assumes it will never
see: not a regression in herdr, but a baseline that encodes noise as truth.

## Disposition

Per the adopted SPEC baseline-refresh policy, cause (1c)/(3): "the baseline
itself is proven wrong". Refresh executed with:
- this finding as the named cause,
- the six-run evidence table above,
- refreshed value = median of the six runs (4000), not another single sample,
- envelope trail: run_id lab-20260822T012136-a2e2f9c9 and siblings under
  `lab/artifacts/bench-output-1-20260822T01*`.

Thresholds unchanged (cliff 1.0 = 2x, band 0.25).
