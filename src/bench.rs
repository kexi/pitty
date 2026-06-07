//! Bench mode: repeat one scenario and report duration statistics plus a
//! pass rate that surfaces flakiness.
//!
//! A bench run executes the existing [`run_scenario`] `warmup + runs` times in
//! sequence (snapshot writes must not race, matching the runner's sequential
//! contract). Warmup iterations are discarded before metrics are computed; the
//! remaining `runs` durations feed a self-contained statistics pass (sort plus
//! arithmetic — no external statistics crate), and the count of passing runs
//! drives a `FLAKY` verdict when it is neither all-pass nor all-fail.

use std::path::Path;

use serde::Serialize;

use crate::config::Scenario;
use crate::error::PittyError;
use crate::report::Status;
use crate::runner::{run_scenario, RunOptions};

/// Duration statistics over the measured (non-warmup) runs, all in milliseconds.
///
/// Serialized into [`BenchReport`] for `--json`. `stddev` is the population
/// standard deviation (divides by `n`, not `n - 1`): bench measures a fixed,
/// fully observed set of runs rather than sampling a larger population, so the
/// population form is the right denominator and also avoids a divide-by-zero for
/// a single run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stats {
    /// Smallest observed duration.
    pub min: u128,
    /// Largest observed duration.
    pub max: u128,
    /// Arithmetic mean, rounded to the nearest integer millisecond.
    pub mean: u128,
    /// Median (middle value, or mean of the two middle values for even counts).
    pub median: u128,
    /// 95th percentile by the nearest-rank method.
    pub p95: u128,
    /// Population standard deviation, rounded to the nearest integer.
    pub stddev: u128,
}

impl Stats {
    /// Compute statistics over `durations` (milliseconds).
    ///
    /// Sorts a local copy once and derives every order statistic from it. Panics
    /// if `durations` is empty: callers guarantee at least one measured run
    /// (`runs >= 1`), so an empty input is a programming error, not user input.
    pub fn from_durations(durations: &[u128]) -> Stats {
        assert!(
            !durations.is_empty(),
            "Stats::from_durations requires at least one duration"
        );
        let mut sorted = durations.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();

        let min = sorted[0];
        let max = sorted[n - 1];

        // Sum in u128 to avoid overflow across many millisecond durations.
        let sum: u128 = sorted.iter().sum();
        let mean = div_round(sum, n as u128);

        let median = median_of_sorted(&sorted);
        let p95 = percentile_nearest_rank(&sorted, 95);
        let stddev = population_stddev(&sorted);

        Stats {
            min,
            max,
            mean,
            median,
            p95,
            stddev,
        }
    }
}

/// Median of an already-sorted slice. For an even count, the mean of the two
/// middle elements (rounded). Assumes a non-empty input.
fn median_of_sorted(sorted: &[u128]) -> u128 {
    let n = sorted.len();
    let mid = n / 2;
    let is_even = n.is_multiple_of(2);
    if is_even {
        // Average the two central values; rounding keeps the integer result
        // closest to the true midpoint.
        div_round(sorted[mid - 1] + sorted[mid], 2)
    } else {
        sorted[mid]
    }
}

/// Percentile by the nearest-rank method on a sorted slice.
///
/// Rank = ceil(p/100 * n), clamped to `[1, n]`, then taken 1-indexed. Why
/// nearest-rank rather than interpolation: it returns an actually-observed
/// duration (no synthetic in-between value) and has a single unambiguous
/// definition, which keeps the bench output reproducible and easy to reason
/// about for small `n`.
fn percentile_nearest_rank(sorted: &[u128], p: u8) -> u128 {
    let n = sorted.len() as u128;
    // ceil(p * n / 100) using integer arithmetic.
    let rank = (p as u128 * n).div_ceil(100);
    // A 0 rank (only possible for p == 0) maps to the first element; otherwise
    // clamp to n so rank == n is the last element. Convert 1-indexed -> 0-indexed.
    let idx = rank.clamp(1, n) - 1;
    sorted[idx as usize]
}

/// Population standard deviation of a slice.
///
/// Uses an unrounded f64 mean internally for accuracy (rather than the report's
/// rounded `mean`) to keep rounding error out of the squared deviation terms,
/// then rounds only the final result.
fn population_stddev(sorted: &[u128]) -> u128 {
    let n = sorted.len();
    let exact_mean = sorted.iter().sum::<u128>() as f64 / n as f64;
    let variance = sorted
        .iter()
        .map(|&d| {
            let delta = d as f64 - exact_mean;
            delta * delta
        })
        .sum::<f64>()
        / n as f64;
    variance.sqrt().round() as u128
}

/// Integer division rounded to nearest (ties away from zero). `denom` must be
/// non-zero; bench always divides by a positive count.
fn div_round(num: u128, denom: u128) -> u128 {
    (num + denom / 2) / denom
}

/// A serializable bench result: the measured durations and their statistics,
/// plus the pass count used for flaky detection.
#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    /// The scenario's name.
    pub scenario: String,
    /// Number of measured (non-warmup) runs.
    pub runs: usize,
    /// Number of discarded warmup runs.
    pub warmup: usize,
    /// How many measured runs passed (all assertions held).
    pub pass_count: usize,
    /// Per-run durations in milliseconds (measured runs only, in run order).
    pub durations: Vec<u128>,
    /// Aggregate duration statistics.
    pub stats: Stats,
}

impl BenchReport {
    /// Whether the scenario is flaky: some but not all measured runs passed.
    ///
    /// All-pass and all-fail are both deterministic verdicts; only a mixed
    /// outcome signals nondeterminism, which is the whole point of bench mode.
    pub fn is_flaky(&self) -> bool {
        self.pass_count > 0 && self.pass_count < self.runs
    }

    /// Serialize to pretty JSON for `--json` output.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self)
            .unwrap_or_else(|_| "{\"error\":\"failed to serialize bench report\"}".to_string())
    }

    /// Render the human-readable summary block written to stdout.
    pub fn to_summary(&self) -> String {
        let flaky = if self.is_flaky() { "  (FLAKY)" } else { "" };
        let stats = &self.stats;
        format!(
            "scenario: {}\n\
             runs: {} ({} warmup)\n\
             pass: {}/{}{}\n\
             duration_ms: min {}  median {}  mean {}  p95 {}  max {}  stddev {}",
            self.scenario,
            self.runs,
            self.warmup,
            self.pass_count,
            self.runs,
            flaky,
            stats.min,
            stats.median,
            stats.mean,
            stats.p95,
            stats.max,
            stats.stddev,
        )
    }
}

/// Execute a scenario `warmup + runs` times and aggregate the measured runs.
///
/// Warmup iterations run the scenario but are excluded from the report. A hard
/// fault ([`PittyError`]) from any iteration aborts the whole bench and is
/// returned to the caller (mapped to its severity exit code), since a process or
/// scenario fault is not something repeating can characterize.
///
/// `runs` must be at least 1; the CLI defaults it to 10 and rejects 0 before
/// reaching here.
pub fn run_bench(
    scenario: &Scenario,
    base_dir: &Path,
    options: &RunOptions,
    runs: usize,
    warmup: usize,
) -> Result<BenchReport, PittyError> {
    assert!(runs >= 1, "run_bench requires runs >= 1");

    // Discard warmup iterations: they prime caches/JITs so the measured runs are
    // not skewed by first-iteration cost.
    for _ in 0..warmup {
        run_scenario(scenario, base_dir, options)?;
    }

    let mut durations = Vec::with_capacity(runs);
    let mut pass_count = 0;
    for _ in 0..runs {
        let report = run_scenario(scenario, base_dir, options)?;
        durations.push(report.duration_ms);
        if report.status == Status::Passed {
            pass_count += 1;
        }
    }

    let stats = Stats::from_durations(&durations);
    Ok(BenchReport {
        scenario: scenario.name.clone(),
        runs,
        warmup,
        pass_count,
        durations,
        stats,
    })
}

/// Map a [`BenchReport`] to a process exit code.
///
/// All measured runs passing yields the `Passed` class (0); any failing run
/// yields the `Failed` class (1): bench exists to observe flakiness, so a
/// single failing run is enough to fail the process. We route through
/// [`crate::report::status_exit_code`] rather than returning bare `0`/`1` so the
/// numeric mapping lives in one exhaustive table (Why not duplicate the
/// literals: a future exit-code renumbering would otherwise have to change here
/// too and could drift). Hard faults never reach here — they are returned as
/// `Err` from [`run_bench`] and mapped via `PittyError::exit_code` by the CLI;
/// this helper handles only the completed-report case.
pub fn bench_exit_code(report: &BenchReport) -> u8 {
    let status = if report.pass_count == report.runs {
        Status::Passed
    } else {
        Status::Failed
    };
    crate::report::status_exit_code(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_single_value_has_zero_stddev_and_equal_order_stats() {
        // A single measurement: every order statistic equals that value and the
        // spread is zero.
        let s = Stats::from_durations(&[1500]);
        assert_eq!(
            s,
            Stats {
                min: 1500,
                max: 1500,
                mean: 1500,
                median: 1500,
                p95: 1500,
                stddev: 0,
            }
        );
    }

    #[test]
    fn stats_identical_values_have_zero_spread() {
        // Many identical values: stddev is 0 and all order stats collapse.
        let s = Stats::from_durations(&[200, 200, 200, 200]);
        assert_eq!(s.min, 200);
        assert_eq!(s.max, 200);
        assert_eq!(s.mean, 200);
        assert_eq!(s.median, 200);
        assert_eq!(s.p95, 200);
        assert_eq!(s.stddev, 0);
    }

    #[test]
    fn median_is_middle_for_odd_count() {
        // Odd count: the median is the single central element after sorting.
        let s = Stats::from_durations(&[30, 10, 20]);
        assert_eq!(s.median, 20);
    }

    #[test]
    fn median_averages_two_centers_for_even_count() {
        // Even count: the median is the rounded mean of the two central values.
        let s = Stats::from_durations(&[10, 20, 30, 40]);
        assert_eq!(s.median, 25); // (20 + 30) / 2
    }

    #[test]
    fn median_even_count_rounds_half_up() {
        // (20 + 25) / 2 = 22.5 must round to 23 (ties away from zero).
        let s = Stats::from_durations(&[10, 20, 25, 40]);
        assert_eq!(s.median, 23);
    }

    #[test]
    fn p95_nearest_rank_on_twenty_values_picks_nineteenth() {
        // For n = 20, ceil(0.95 * 20) = 19, so p95 is the 19th value (1-indexed)
        // of the sorted sequence 1..=20, i.e. 19.
        let durations: Vec<u128> = (1..=20).collect();
        let s = Stats::from_durations(&durations);
        assert_eq!(s.p95, 19);
    }

    #[test]
    fn p95_nearest_rank_rounds_up_for_ten_values() {
        // For n = 10, ceil(0.95 * 10) = ceil(9.5) = 10, so p95 is the max.
        let durations: Vec<u128> = (1..=10).collect();
        let s = Stats::from_durations(&durations);
        assert_eq!(s.p95, 10);
    }

    #[test]
    fn p95_of_single_value_is_that_value() {
        // n = 1: ceil(0.95) = 1, clamped to the only element.
        let s = Stats::from_durations(&[42]);
        assert_eq!(s.p95, 42);
    }

    #[test]
    fn mean_and_stddev_match_hand_computed_values() {
        // For [2,4,4,4,5,5,7,9]: mean 5, population stddev 2 exactly.
        let s = Stats::from_durations(&[2, 4, 4, 4, 5, 5, 7, 9]);
        assert_eq!(s.mean, 5);
        assert_eq!(s.stddev, 2);
    }

    #[test]
    fn min_max_track_extremes_regardless_of_input_order() {
        // Order statistics must not depend on input order.
        let s = Stats::from_durations(&[1180, 2100, 1340, 1980]);
        assert_eq!(s.min, 1180);
        assert_eq!(s.max, 2100);
    }

    /// Build a BenchReport with a given pass_count/runs for flaky tests.
    fn report_with(pass_count: usize, runs: usize) -> BenchReport {
        BenchReport {
            scenario: "x".into(),
            runs,
            warmup: 0,
            pass_count,
            durations: vec![1; runs],
            stats: Stats::from_durations(&[1]),
        }
    }

    #[test]
    fn flaky_only_when_some_but_not_all_pass() {
        // Mixed pass/fail is flaky; all-pass and all-fail are not.
        assert!(report_with(9, 10).is_flaky());
        assert!(report_with(1, 10).is_flaky());
        assert!(!report_with(10, 10).is_flaky());
        assert!(!report_with(0, 10).is_flaky());
    }

    #[test]
    fn exit_code_zero_only_when_all_runs_pass() {
        // All-pass -> 0; any fail -> assertion class 1.
        assert_eq!(bench_exit_code(&report_with(10, 10)), 0);
        assert_eq!(bench_exit_code(&report_with(9, 10)), 1);
        assert_eq!(bench_exit_code(&report_with(0, 10)), 1);
    }

    #[test]
    fn fault_maps_straight_to_exit_code_class() {
        // A hard fault from a bench iteration must reach the CLI as its plain
        // class code (scenario 2 / process 3), the same conversion matrix uses.
        assert_eq!(PittyError::Scenario("x".into()).exit_code(), 2);
        assert_eq!(PittyError::Process("x".into()).exit_code(), 3);
    }

    #[test]
    fn summary_marks_flaky_and_lists_stats() {
        // The human summary must show the pass ratio, the FLAKY marker when
        // applicable, and the duration line.
        let mut r = report_with(9, 10);
        r.durations = vec![1180, 1340, 1980, 2100];
        r.stats = Stats::from_durations(&r.durations);
        let out = r.to_summary();
        assert!(out.contains("pass: 9/10"));
        assert!(out.contains("(FLAKY)"));
        assert!(out.contains("duration_ms:"));
        assert!(out.contains("min 1180"));
        assert!(out.contains("max 2100"));
    }

    #[test]
    fn summary_omits_flaky_when_deterministic() {
        // An all-pass report must not carry the FLAKY marker.
        let r = report_with(10, 10);
        assert!(!r.to_summary().contains("FLAKY"));
    }

    #[test]
    fn p95_two_values_nearest_rank_is_the_upper() {
        // (R-4) n = 2 boundary: ceil(0.95 * 2) = ceil(1.9) = 2, so p95 is the
        // 2nd (last) sorted value, the upper of the pair — not interpolated.
        let s = Stats::from_durations(&[10, 1000]);
        assert_eq!(s.p95, 1000);
    }

    #[test]
    fn all_fail_is_not_flaky_and_exits_one() {
        // (R-4) A scenario that fails every measured run is deterministically
        // failing, not flaky (flaky requires a mixed outcome). It must still fail
        // the process with the assertion class (1).
        let r = report_with(0, 4);
        assert!(!r.is_flaky(), "all-fail is deterministic, not flaky");
        assert_eq!(bench_exit_code(&r), 1);
    }

    #[test]
    fn warmup_greater_than_runs_measures_only_runs() {
        // (R-3) warmup >= runs must not panic or underflow: with runs = 1 and
        // warmup = 3, exactly one measured run is recorded (the three warmups are
        // discarded) and the statistics pass succeeds over that single value.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("present.txt"), b"x").unwrap();
        // A spawn-less, file-only scenario runs without a PTY, so this is a unit
        // test rather than a PTY-gated e2e: it isolates the warmup/runs counting.
        let yaml = "name: warmup-heavy\nsteps:\n  - expect_file_exists:\n      path: present.txt\n";
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let report = run_bench(&scenario, dir.path(), &RunOptions::default(), 1, 3).unwrap();
        assert_eq!(report.runs, 1);
        assert_eq!(report.warmup, 3);
        assert_eq!(report.durations.len(), 1, "only measured runs are recorded");
        assert_eq!(report.pass_count, 1);
        assert!(!report.is_flaky());
    }

    #[test]
    fn bench_report_serializes_durations_and_stats() {
        // The JSON form must carry the measured durations and nested stats.
        let mut r = report_with(10, 3);
        r.durations = vec![10, 20, 30];
        r.stats = Stats::from_durations(&r.durations);
        let json = r.to_json();
        assert!(json.contains("\"durations\""));
        assert!(json.contains("\"stats\""));
        assert!(json.contains("\"p95\""));
        assert!(json.contains("\"pass_count\": 10"));
    }
}
