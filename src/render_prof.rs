//! Lightweight opt-in profiling for local performance investigations.
//!
//! Enable with `HERDR_RENDER_PROF=1`; one summary line per second is written to
//! the session log under `event="render.prof"`. Names are fixed `&'static str`
//! literals rather than per-pane labels, so cost does not scale with the
//! session, and every recording path is a no-op when the variable is unset.
//!
//! Four kinds of measurement, which differ in what they can answer:
//!
//! - [`counter`]/[`event`] — how often something happened.
//! - [`duration`] — count, average and maximum. Cheap; the right default.
//! - [`histogram`] — adds p50/p95/p99. Use when the tail is the finding, since
//!   an average over a mostly-fast distribution reports neither the common case
//!   nor the bad one.
//! - [`gauge`] — a sampled level with its peak retained, for queue depth and
//!   queue bytes, which a monotonic counter cannot describe.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ENV_VAR: &str = "HERDR_RENDER_PROF";

static ENABLED: OnceLock<bool> = OnceLock::new();
static PROFILER: OnceLock<Mutex<RenderProfiler>> = OnceLock::new();

/// Upper bounds, in nanoseconds, of every [`Histogram`] bucket except the last.
///
/// Values above the final bound land in an overflow bucket. The spread is
/// deliberately wide: the same histogram has to describe sub-millisecond render
/// phases and multi-second stalls caused by a blocking socket write.
const HISTOGRAM_BOUNDS_NS: [u128; 16] = [
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    25_000_000,
    50_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    1_000_000_000,
    2_500_000_000,
    5_000_000_000,
];

#[derive(Default)]
struct DurationStats {
    count: u64,
    total_ns: u128,
    max_ns: u128,
}

/// Bucketed durations, for the cases where an average hides the problem.
///
/// Latency findings are stated as percentiles because that is what a stall
/// looks like: a p50 that never moves while the p99 sits at seconds. Counting
/// into fixed buckets keeps the recording allocation-free on the measured path.
struct Histogram {
    buckets: [u64; HISTOGRAM_BOUNDS_NS.len() + 1],
    count: u64,
    total_ns: u128,
    max_ns: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: [0; HISTOGRAM_BOUNDS_NS.len() + 1],
            count: 0,
            total_ns: 0,
            max_ns: 0,
        }
    }
}

impl Histogram {
    fn record(&mut self, duration: Duration) {
        let ns = duration.as_nanos();
        let index = HISTOGRAM_BOUNDS_NS
            .iter()
            .position(|bound| ns <= *bound)
            .unwrap_or(HISTOGRAM_BOUNDS_NS.len());
        self.buckets[index] += 1;
        self.count += 1;
        self.total_ns += ns;
        self.max_ns = self.max_ns.max(ns);
    }

    /// Returns the bucket upper bound holding the `percentile`th observation.
    ///
    /// Reporting the bound rather than an interpolated value keeps this honest
    /// about its own resolution — it never claims precision the buckets do not
    /// have. An observation in the overflow bucket reports the true maximum,
    /// so an unbounded stall is never understated.
    fn percentile_ns(&self, percentile: f64) -> u128 {
        if self.count == 0 {
            return 0;
        }
        let rank = (percentile * self.count as f64).ceil().max(1.0) as u64;
        let mut cumulative = 0;
        for (index, bucket) in self.buckets.iter().enumerate() {
            cumulative += bucket;
            if cumulative >= rank {
                return HISTOGRAM_BOUNDS_NS
                    .get(index)
                    .copied()
                    .unwrap_or(self.max_ns)
                    .min(self.max_ns);
            }
        }
        self.max_ns
    }
}

/// A sampled level rather than a running total.
///
/// Queue depth and queue bytes are the motivating case: the useful facts are
/// how deep it is now and how deep it ever got, neither of which a monotonic
/// counter can answer.
#[derive(Default)]
struct GaugeStats {
    last: u64,
    max: u64,
    samples: u64,
}

struct RenderProfiler {
    window_started: Instant,
    counters: BTreeMap<&'static str, u64>,
    durations: BTreeMap<&'static str, DurationStats>,
    histograms: BTreeMap<&'static str, Histogram>,
    gauges: BTreeMap<&'static str, GaugeStats>,
}

impl RenderProfiler {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            counters: BTreeMap::new(),
            durations: BTreeMap::new(),
            histograms: BTreeMap::new(),
            gauges: BTreeMap::new(),
        }
    }

    fn increment(&mut self, name: &'static str, value: u64) {
        *self.counters.entry(name).or_default() += value;
    }

    fn duration(&mut self, name: &'static str, duration: Duration) {
        let stats = self.durations.entry(name).or_default();
        let ns = duration.as_nanos();
        stats.count += 1;
        stats.total_ns += ns;
        stats.max_ns = stats.max_ns.max(ns);
    }

    fn histogram(&mut self, name: &'static str, duration: Duration) {
        self.histograms.entry(name).or_default().record(duration);
    }

    fn gauge(&mut self, name: &'static str, value: u64) {
        let stats = self.gauges.entry(name).or_default();
        stats.last = value;
        stats.max = stats.max.max(value);
        stats.samples += 1;
    }

    fn flush_if_due(&mut self) {
        let elapsed = self.window_started.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }

        let counters = self
            .counters
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(",");
        let durations = self
            .durations
            .iter()
            .map(|(name, stats)| {
                let avg_us = if stats.count == 0 {
                    0
                } else {
                    stats.total_ns / u128::from(stats.count) / 1_000
                };
                let max_us = stats.max_ns / 1_000;
                // `avg_us` truncates, which is useless for anything whose average is
                // under a microsecond and called hundreds of thousands of times per
                // window. The total keeps that answer recoverable.
                let total_us = stats.total_ns / 1_000;
                format!(
                    "{name}=count:{} avg_us:{} total_us:{} max_us:{}",
                    stats.count, avg_us, total_us, max_us
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let histograms = self
            .histograms
            .iter()
            .map(|(name, stats)| {
                let avg_us = if stats.count == 0 {
                    0
                } else {
                    stats.total_ns / u128::from(stats.count) / 1_000
                };
                format!(
                    "{name}=count:{} avg_us:{} p50_us:{} p95_us:{} p99_us:{} max_us:{}",
                    stats.count,
                    avg_us,
                    stats.percentile_ns(0.50) / 1_000,
                    stats.percentile_ns(0.95) / 1_000,
                    stats.percentile_ns(0.99) / 1_000,
                    stats.max_ns / 1_000,
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let gauges = self
            .gauges
            .iter()
            .map(|(name, stats)| {
                format!(
                    "{name}=last:{} max:{} samples:{}",
                    stats.last, stats.max, stats.samples
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        tracing::info!(
            event = "render.prof",
            window_ms = elapsed.as_millis() as u64,
            counters = %counters,
            durations = %durations,
            histograms = %histograms,
            gauges = %gauges,
            "render profiler window"
        );

        self.window_started = Instant::now();
        self.counters.clear();
        self.durations.clear();
        self.histograms.clear();
        // Gauges keep their max across windows deliberately: the peak queue
        // depth of a run is the interesting number, and it is easy to miss if
        // it lands in a window nobody reads.
        for stats in self.gauges.values_mut() {
            stats.samples = 0;
        }
    }
}

pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var(ENV_VAR)
            .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false)
    })
}

fn with_profiler(update: impl FnOnce(&mut RenderProfiler)) {
    if !enabled() {
        return;
    }
    let profiler = PROFILER.get_or_init(|| Mutex::new(RenderProfiler::new()));
    if let Ok(mut profiler) = profiler.lock() {
        update(&mut profiler);
    }
}

pub(crate) fn counter(name: &'static str, value: u64) {
    if value == 0 {
        return;
    }
    with_profiler(|profiler| profiler.increment(name, value));
}

pub(crate) fn event(name: &'static str) {
    counter(name, 1);
}

pub(crate) fn duration(name: &'static str, duration: Duration) {
    with_profiler(|profiler| profiler.duration(name, duration));
}

/// Records `duration` into a bucketed histogram reported with percentiles.
///
/// Prefer this over [`duration`] when the finding is about tail latency; prefer
/// [`duration`] when the average is the point and the extra buckets are not.
pub(crate) fn histogram(name: &'static str, duration: Duration) {
    with_profiler(|profiler| profiler.histogram(name, duration));
}

/// Samples a level, such as a queue's current depth or byte size.
pub(crate) fn gauge(name: &'static str, value: u64) {
    with_profiler(|profiler| profiler.gauge(name, value));
}

pub(crate) fn timer() -> Option<Instant> {
    enabled().then(Instant::now)
}

pub(crate) fn duration_since(name: &'static str, started: Option<Instant>) {
    if let Some(started) = started {
        duration(name, started.elapsed());
    }
}

pub(crate) fn histogram_since(name: &'static str, started: Option<Instant>) {
    if let Some(started) = started {
        histogram(name, started.elapsed());
    }
}

pub(crate) fn flush_if_due() {
    with_profiler(RenderProfiler::flush_if_due);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn micros(value: u64) -> Duration {
        Duration::from_micros(value)
    }

    #[test]
    fn empty_histogram_reports_zero_rather_than_panicking() {
        let histogram = Histogram::default();
        assert_eq!(histogram.percentile_ns(0.50), 0);
        assert_eq!(histogram.percentile_ns(0.99), 0);
    }

    #[test]
    fn percentiles_separate_a_slow_tail_from_a_fast_median() {
        // The shape every latency finding in the analysis describes: almost
        // everything is fast, and the tail is the whole problem. An average
        // would report roughly 5 ms here and hide both facts.
        let mut histogram = Histogram::default();
        for _ in 0..99 {
            histogram.record(micros(60));
        }
        histogram.record(Duration::from_millis(500));

        assert_eq!(histogram.percentile_ns(0.50), 100_000);
        assert_eq!(histogram.percentile_ns(0.99), 100_000);
        assert_eq!(histogram.max_ns, 500_000_000);
    }

    #[test]
    fn percentile_never_exceeds_the_observed_maximum() {
        // The bucket bound is an upper estimate, so a lone fast observation
        // must still not report the bound it happens to fall under.
        let mut histogram = Histogram::default();
        histogram.record(micros(1));

        assert_eq!(histogram.percentile_ns(0.99), 1_000);
    }

    #[test]
    fn overflow_observations_report_the_true_maximum() {
        // Above the last bound there is no bucket to name, and understating an
        // unbounded stall is the one failure mode that matters here.
        let mut histogram = Histogram::default();
        histogram.record(Duration::from_secs(30));

        assert_eq!(histogram.percentile_ns(0.99), 30_000_000_000);
    }

    #[test]
    fn every_observation_lands_in_exactly_one_bucket() {
        let mut histogram = Histogram::default();
        for value in [1, 50, 51, 999_999, 4_000_000, 60_000_000] {
            histogram.record(micros(value));
        }

        assert_eq!(histogram.count, 6);
        assert_eq!(histogram.buckets.iter().sum::<u64>(), 6);
    }

    #[test]
    fn bucket_bounds_are_ascending() {
        // percentile_ns walks the buckets in order and stops at the first one
        // that covers the rank; unordered bounds would silently misreport.
        assert!(HISTOGRAM_BOUNDS_NS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn gauge_tracks_the_current_level_and_the_peak_separately() {
        let mut profiler = RenderProfiler::new();
        profiler.gauge("queue.bytes", 10);
        profiler.gauge("queue.bytes", 4_096);
        profiler.gauge("queue.bytes", 0);

        let stats = &profiler.gauges["queue.bytes"];
        assert_eq!(stats.last, 0, "a drained queue reads as empty now");
        assert_eq!(stats.max, 4_096, "but the peak it reached is retained");
        assert_eq!(stats.samples, 3);
    }

    #[test]
    fn flushing_retains_gauge_peaks_but_clears_histograms() {
        let mut profiler = RenderProfiler::new();
        profiler.gauge("queue.bytes", 4_096);
        profiler.histogram("loop.active", micros(500));
        profiler.window_started = Instant::now() - Duration::from_secs(2);

        profiler.flush_if_due();

        assert!(profiler.histograms.is_empty());
        assert_eq!(profiler.gauges["queue.bytes"].max, 4_096);
        assert_eq!(profiler.gauges["queue.bytes"].samples, 0);
    }
}
