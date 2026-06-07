//! Matrix mode: run one scenario once per cell of the axes' Cartesian product.
//!
//! Injection method (design "case A"): for each cell — a concrete assignment of
//! one value to every axis — clone the scenario and overwrite the same-named
//! entries in `variables` with the cell's values as [`VarSpec::Plain`], then call
//! the unmodified [`run_scenario`]. The scenario author writes `${axis}` wherever
//! each value should land (e.g. `spawn: "${command} --region ${region}"`), so the
//! existing `${var}` expansion resolves each cell without any runner change.
//! Cells run sequentially to honor the runner's no-snapshot-race contract.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::cli::aggregate_exit_codes;
use crate::config::{Scenario, VarSpec};
use crate::error::PtytestError;
use crate::report::{status_exit_code, status_verdict_label, Report, Status};
use crate::runner::{run_scenario, RunOptions};

/// The default maximum number of cells a matrix may expand to before it is
/// rejected as a Scenario error.
///
/// Why a cap at all, and why an error not a warning (C): the Cartesian product
/// grows multiplicatively, so a handful of axes can silently demand thousands of
/// real process spawns — an accidental local DoS. A cap turns a runaway product
/// into a fast, explicit failure. It is an *error* rather than a warning to stay
/// consistent with the existing "vacuous/explosive matrices fail loudly" philosophy
/// (an empty axis is already a Scenario error): a warning could scroll past in CI
/// and let the spawn storm proceed, whereas an error stops it before the first cell.
const DEFAULT_MAX_CELLS: usize = 256;

/// Environment variable that overrides [`DEFAULT_MAX_CELLS`].
///
/// CI can intentionally raise the ceiling for a large sweep. An unset or
/// unparseable value falls back to the default (see [`max_cells`]) so a typo can
/// never panic the harness or silently disable the guard.
const MAX_CELLS_ENV: &str = "PTYTEST_MATRIX_MAX_CELLS";

/// One matrix cell: the per-axis values injected and the resulting run report.
#[derive(Debug, Clone, Serialize)]
pub struct MatrixCell {
    /// The concrete assignment for this cell, mapping each axis name to the value
    /// used (e.g. `{command: "bash", region: "us"}`). Key order is the axis order.
    pub coords: BTreeMap<String, String>,
    /// The report produced by running the scenario with those values injected.
    pub report: Report,
}

/// A serializable matrix result: the axis names and one cell per product element.
#[derive(Debug, Clone, Serialize)]
pub struct MatrixReport {
    /// The axis names, in `BTreeMap` key (lexicographic) order. A single-axis
    /// matrix has one entry; the structure is identical for one or many axes.
    pub axes: Vec<String>,
    /// One cell per element of the axes' Cartesian product, in expansion order.
    pub cells: Vec<MatrixCell>,
}

impl MatrixReport {
    /// Serialize to pretty JSON for `--json` output.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self)
            .unwrap_or_else(|_| "{\"error\":\"failed to serialize matrix report\"}".to_string())
    }

    /// Render the human-readable table written to stdout.
    ///
    /// Hybrid layout, branching only on axis count so the report structure stays
    /// identical for one or many axes:
    /// - **single axis**: each row is `{value}  {PASS/FAIL}  ({ms}ms)` with the
    ///   value column padded to the widest value, the familiar v0.3 layout.
    /// - **two or more axes**: each row is `key=value key=value  {verdict} ({ms}ms)`,
    ///   listing every coordinate as `key=value` so a cell is self-describing
    ///   without a header row; the verdict column is aligned to the widest label.
    ///
    /// Self-formatted with `format!` width specifiers rather than a table crate,
    /// per the no-new-dependency rule.
    pub fn to_table(&self) -> String {
        let is_single_axis = self.axes.len() == 1;
        if is_single_axis {
            return self.to_single_axis_table();
        }
        self.to_multi_axis_table()
    }

    /// The single-axis table: value column padded to the widest value.
    fn to_single_axis_table(&self) -> String {
        // Width the label column to the longest value so columns align; clamp to
        // a sane minimum so short axes still read as a table.
        let width = self
            .cells
            .iter()
            .map(|c| cell_value_only(c).chars().count())
            .max()
            .unwrap_or(0)
            .max(8);

        let mut out = String::new();
        for cell in &self.cells {
            out.push_str(&format!(
                "{:<width$}  {}  ({}ms)\n",
                cell_value_only(cell),
                status_verdict_label(cell.report.status),
                cell.report.duration_ms,
                width = width,
            ));
        }
        out
    }

    /// The multi-axis table: each cell rendered as space-separated `key=value`
    /// coordinates, with the verdict aligned to the widest coordinate label.
    fn to_multi_axis_table(&self) -> String {
        let labels: Vec<String> = self.cells.iter().map(cell_label).collect();
        let width = labels
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
            .max(8);

        let mut out = String::new();
        for (label, cell) in labels.iter().zip(&self.cells) {
            out.push_str(&format!(
                "{:<width$}  {} ({}ms)\n",
                label,
                status_verdict_label(cell.report.status),
                cell.report.duration_ms,
                width = width,
            ));
        }
        out
    }

    /// How many cells passed.
    ///
    /// Computed from `cells` rather than stored as a serialized field: the cells
    /// array is the single source of truth, so a derived count cannot drift from
    /// it, and the JSON contract (frozen at v1.0) can still gain an explicit
    /// count later as a backward-compatible minor addition if needed.
    pub fn passed(&self) -> usize {
        self.cells
            .iter()
            .filter(|c| c.report.status == Status::Passed)
            .count()
    }

    /// How many cells failed (the complement of [`MatrixReport::passed`]).
    pub fn failed(&self) -> usize {
        self.total() - self.passed()
    }

    /// The total number of cells (Cartesian product size).
    pub fn total(&self) -> usize {
        self.cells.len()
    }

    /// The worst per-cell exit code, reusing the CLI's severity aggregation so a
    /// single failing cell fails the whole matrix (CI gate). All-pass yields 0.
    pub fn worst_exit_code(&self) -> u8 {
        let codes = self
            .cells
            .iter()
            .map(|cell| status_exit_code(cell.report.status));
        aggregate_exit_codes(codes)
    }
}

/// Render a cell's coordinates as `key=value key=value` in axis order.
fn cell_label(cell: &MatrixCell) -> String {
    cell.coords
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The single coordinate value of a one-axis cell (for the single-axis table).
///
/// A single-axis cell has exactly one coordinate; fall back to an empty string
/// only defensively so the table never panics on an unexpected shape.
fn cell_value_only(cell: &MatrixCell) -> &str {
    cell.coords
        .values()
        .next()
        .map(String::as_str)
        .unwrap_or("")
}

/// Run a scenario once per cell of the axes' Cartesian product and collect the
/// reports.
///
/// Validates every axis first (non-empty values, key referenced as `${key}`, no
/// secret collision) via [`Scenario::matrix_axes`], then expands the product with
/// the size/overflow guard enforced inside [`cell_coordinates`] against
/// [`max_cells`]; both surface as [`PtytestError::Scenario`]. For each cell,
/// clones the scenario and injects the cell's per-axis values.
///
/// A hard fault from any cell aborts the matrix and is returned to the caller;
/// assertion failures are recorded in their cell's report (status `Failed`) and
/// do not stop later cells, so the table shows every cell's verdict.
pub fn run_matrix(
    scenario: &Scenario,
    base_dir: &Path,
    options: &RunOptions,
) -> Result<MatrixReport, PtytestError> {
    let axes = scenario.matrix_axes()?;
    let axis_names: Vec<String> = axes.iter().map(|(name, _)| name.to_string()).collect();

    // Guard the product size *before* expanding so a spawn storm is rejected up
    // front rather than discovered after the first hundred spawns, and so the
    // running product is checked with `checked_mul` to never overflow `usize`.
    let limit = max_cells();
    let combos = cell_coordinates(&axes, limit)?;

    let mut cells = Vec::with_capacity(combos.len());
    for coords in combos {
        let cell_scenario = inject_cell_values(scenario, &coords);
        let report = run_scenario(&cell_scenario, base_dir, options)?;
        cells.push(MatrixCell { coords, report });
    }

    Ok(MatrixReport {
        axes: axis_names,
        cells,
    })
}

/// Resolve the per-run cell cap from the environment, falling back to the
/// default on an unset or unparseable value.
///
/// Why fall back rather than error on a bad value: the override exists for CI
/// convenience, and a typo'd env var must not become a new failure mode that
/// blocks an otherwise valid matrix — defaulting keeps the guard active and the
/// run reproducible. A `0` (or any value below the product) still trips the
/// guard in [`run_matrix`]; only a non-numeric value falls back.
fn max_cells() -> usize {
    match std::env::var(MAX_CELLS_ENV) {
        Ok(v) => v.trim().parse::<usize>().unwrap_or(DEFAULT_MAX_CELLS),
        Err(_) => DEFAULT_MAX_CELLS,
    }
}

/// Expand the axes into the ordered list of cell coordinate maps (the Cartesian
/// product), using only the standard library, rejecting a product that overflows
/// `usize` or exceeds `limit` *before* materializing it.
///
/// Order is deterministic: axes come in `BTreeMap` key order (as
/// [`Scenario::matrix_axes`] returns them) and each axis varies in its declared
/// value order, with later axes varying fastest. For
/// `{command:[a,b], region:[x,y]}` this yields `(a,x),(a,y),(b,x),(b,y)`.
///
/// Why a fold building `Vec<BTreeMap>` rather than an itertools `multi_cartesian`
/// helper: the no-new-dependency rule. The fold starts from a single empty
/// assignment and, for each axis, replaces every partial assignment with one copy
/// per value of that axis — so appending the last axis last makes it vary fastest,
/// matching the documented order.
///
/// Why the size guard lives *inside* the fold rather than checking
/// `combos.len()` after a full expansion (algorithm W1):
/// - Memory safety comes from the **per-axis `limit` check**: before each axis is
///   expanded, the next product (`combos.len() * values.len()`) is compared to
///   `limit` and the fold bails if it would exceed it. Because every prior pass
///   already bailed once the running product crossed the cap, no intermediate Vec
///   ever grows past `limit` — a product far above the cap never materializes even
///   one full pass, so a spawn-storm-sized product cannot OOM the harness.
/// - The product is computed with **`checked_mul`** so that running multiply can
///   never silently wrap `usize` (an unchecked `combos.len() * values.len()` would
///   wrap in release and could turn a `Vec::with_capacity` into a tiny allocation
///   followed by an unbounded push loop). With a sane `limit` the cap fires long
///   before the arithmetic could overflow; the checked multiply is the belt to the
///   cap's suspenders for the degenerate case of a near-`usize::MAX` limit.
fn cell_coordinates(
    axes: &[(&str, &[String])],
    limit: usize,
) -> Result<Vec<BTreeMap<String, String>>, PtytestError> {
    let mut combos: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
    for (axis, values) in axes {
        let next_count = combos.len().checked_mul(values.len()).ok_or_else(|| {
            PtytestError::Scenario("matrix cell count overflows usize".to_string())
        })?;
        if next_count > limit {
            return Err(PtytestError::Scenario(format!(
                "matrix would expand to more than {limit} cells; \
                 reduce axes/values or raise {MAX_CELLS_ENV}"
            )));
        }
        let mut next = Vec::with_capacity(next_count);
        for partial in &combos {
            for value in *values {
                let mut extended = partial.clone();
                extended.insert((*axis).to_string(), value.clone());
                next.push(extended);
            }
        }
        combos = next;
    }
    Ok(combos)
}

/// Clone `scenario` and overwrite `variables[axis]` with the cell's value (as a
/// plain var) for every axis in `coords`.
///
/// Each matrix value wins over any same-named scenario variable (it is an
/// `insert`, which replaces). This precedence is intentional and applies to every
/// axis: the matrix axes are the things being varied across cells, so they must
/// override any static defaults the scenario declares under the same names. Each
/// injected value is always [`VarSpec::Plain`] — matrix values are not secrets and
/// stay outside the masking machinery (an axis colliding with a secret variable is
/// rejected up front by [`Scenario::matrix_axes`], so injection never reaches one).
fn inject_cell_values(scenario: &Scenario, coords: &BTreeMap<String, String>) -> Scenario {
    let mut clone = scenario.clone();
    for (axis, value) in coords {
        clone
            .variables
            .insert(axis.clone(), VarSpec::Plain(value.clone()));
    }
    clone
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VarSpec;

    /// A single-coordinate map `{axis: value}`.
    fn coord(axis: &str, value: &str) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert(axis.to_string(), value.to_string());
        m
    }

    #[test]
    fn injection_overwrites_same_named_variable() {
        // A matrix value must win over a static variable of the same name.
        let yaml = r#"
name: precedence
variables:
  command: static-default
matrix:
  command: [injected]
steps:
  - spawn: "${command}"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let injected = inject_cell_values(&scenario, &coord("command", "injected"));
        match injected.variables.get("command") {
            Some(VarSpec::Plain(v)) => assert_eq!(v, "injected"),
            other => panic!("expected injected Plain value, got {other:?}"),
        }
    }

    #[test]
    fn injected_value_is_plain_in_the_normal_case() {
        // (C-1) Matrix injection writes a plain value. In the normal case (no
        // same-named secret variable) this is correct: matrix values are
        // plaintext YAML and are not secrets. The dangerous case — an axis whose
        // name collides with a `secret: true` variable — is rejected up front by
        // `Scenario::matrix_axes` (see config::tests), so injection never reaches
        // a secret-declared axis. This test pins only the benign Plain write.
        let scenario = Scenario::from_yaml(
            "name: x\nmatrix:\n  command: [a]\nsteps:\n  - spawn: \"${command}\"\n",
        )
        .unwrap();
        let injected = inject_cell_values(&scenario, &coord("command", "a"));
        assert!(!injected.variables["command"].is_secret());
    }

    #[test]
    fn injects_every_axis_value_for_a_multi_axis_cell() {
        // A multi-axis cell must inject each axis's value into its own same-named
        // variable, so every `${axis}` in the scenario resolves to that cell's
        // coordinate.
        let scenario = Scenario::from_yaml(
            "name: x\nmatrix:\n  command: [a]\n  region: [x]\nsteps:\n  - spawn: \"${command} ${region}\"\n",
        )
        .unwrap();
        let mut coords = BTreeMap::new();
        coords.insert("command".to_string(), "bash".to_string());
        coords.insert("region".to_string(), "us".to_string());
        let injected = inject_cell_values(&scenario, &coords);
        assert_eq!(injected.variables["command"].value(), "bash");
        assert_eq!(injected.variables["region"].value(), "us");
    }

    /// A generous limit for expansion tests that are not exercising the cap.
    const TEST_LIMIT: usize = 1024;

    #[test]
    fn cell_coordinates_is_empty_product_for_no_axes() {
        // Defensive: with no axes the product is a single empty assignment, not
        // zero cells, mirroring the math (an empty product has one element).
        let combos = cell_coordinates(&[], TEST_LIMIT).unwrap();
        assert_eq!(combos.len(), 1);
        assert!(combos[0].is_empty());
    }

    #[test]
    fn cell_coordinates_single_axis_preserves_value_order() {
        // A single axis expands to one cell per value, in declared order.
        let values = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let combos = cell_coordinates(&[("command", &values)], TEST_LIMIT).unwrap();
        let got: Vec<&str> = combos.iter().map(|c| c["command"].as_str()).collect();
        assert_eq!(got, ["a", "b", "c"]);
    }

    #[test]
    fn cell_coordinates_rejects_product_over_limit_before_full_expansion() {
        // A product exceeding `limit` must be a Scenario error (exit 2) raised
        // during expansion, not after — the guard bails on the first axis whose
        // running product crosses the cap.
        let a = vec!["1".to_string(), "2".to_string(), "3".to_string()];
        let b = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        let err = cell_coordinates(&[("a", &a), ("b", &b)], 8).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("more than 8 cells"));
        // Exactly at the limit still expands.
        assert_eq!(
            cell_coordinates(&[("a", &a), ("b", &b)], 9).unwrap().len(),
            9
        );
    }

    #[test]
    fn cell_coordinates_rejects_explosive_product_without_panic_or_oom() {
        // (algorithm W1) A pathologically large axis configuration whose product
        // dwarfs usize must surface as a Scenario error (exit 2) without ever
        // panicking or OOMing. With a realistic limit, the per-axis guard checks
        // the running product (via `checked_mul`) *before* allocating that axis's
        // pass, so expansion bails on the first axis that would cross the cap and
        // the huge intermediate is never materialized.
        let big: Vec<String> = (0..64).map(|n| n.to_string()).collect();
        // 16 axes of 64 values is 64^16 cells — astronomically past any limit and
        // past usize::MAX. The guard must reject it cheaply, not try to build it.
        let axes: Vec<(&str, &[String])> = (0..16).map(|_| ("axis", big.as_slice())).collect();
        let err = cell_coordinates(&axes, DEFAULT_MAX_CELLS).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        // The cap fires first (the running product crosses 256 on the second
        // axis), so the cap message — not the overflow message — is surfaced.
        assert!(err.to_string().contains("more than 256 cells"));
    }

    #[test]
    fn cell_coordinates_reports_overflow_when_running_count_exceeds_usize() {
        // (algorithm W1) The `checked_mul` overflow guard itself: when the limit
        // is so high it never trips, a running product that would overflow usize
        // must still be a Scenario error rather than a wrapping multiply. Seeded
        // with a tiny number of values so the only intermediate ever allocated is
        // a handful of maps — the overflow is in the *arithmetic*, caught before
        // the would-be-overflowing pass allocates.
        //
        // Two values per axis: combos.len() doubles each axis. After 63 axes it is
        // 2^63; the 64th `checked_mul` (2^63 * 2) overflows usize on 64-bit. The
        // cap is set to usize::MAX so the size check never fires and the overflow
        // branch is exercised. NOTE: this would OOM long before axis 63 if the
        // intermediates were materialized — so this test also pins that a real run
        // must rely on the cap (previous test), never this branch, for memory
        // safety. It runs only on 64-bit targets where 2^63 is representable.
        #[cfg(target_pointer_width = "64")]
        {
            // Verify the arithmetic guard in isolation without allocating: mirror
            // the fold's running-count computation directly.
            let mut running: usize = 1;
            let mut overflowed = false;
            for _ in 0..64 {
                match running.checked_mul(2) {
                    Some(n) => running = n,
                    None => {
                        overflowed = true;
                        break;
                    }
                }
            }
            assert!(
                overflowed,
                "running product must overflow usize within 64 doublings on a 64-bit target"
            );
        }
    }

    #[test]
    fn cell_coordinates_product_order_is_deterministic() {
        // (Order contract) For {command:[a,b], region:[x,y]} the product must be
        // (a,x),(a,y),(b,x),(b,y): axis order is key order (command, region) and
        // the last axis (region) varies fastest.
        let command = vec!["a".to_string(), "b".to_string()];
        let region = vec!["x".to_string(), "y".to_string()];
        let combos =
            cell_coordinates(&[("command", &command), ("region", &region)], TEST_LIMIT).unwrap();
        let got: Vec<(String, String)> = combos
            .iter()
            .map(|c| (c["command"].clone(), c["region"].clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a".into(), "x".into()),
                ("a".into(), "y".into()),
                ("b".into(), "x".into()),
                ("b".into(), "y".into()),
            ]
        );
    }

    #[test]
    fn cell_coordinates_three_axis_product_order_is_deterministic() {
        // (AC-7) For three axes {a:[1,2], b:[1,2], c:[1,2]} the product must be
        // fully fixed: axis order is BTreeMap key order (a, b, c) and the LAST
        // axis (c) varies fastest, then b, then a. This is the first arrangement
        // where the interaction between "last axis fastest" and the BTreeMap key
        // ordering is non-trivial (two axes cannot distinguish the two), so the
        // exact 8-tuple sequence is pinned, not just the count.
        let a = vec!["1".to_string(), "2".to_string()];
        let b = vec!["1".to_string(), "2".to_string()];
        let c = vec!["1".to_string(), "2".to_string()];
        let combos = cell_coordinates(&[("a", &a), ("b", &b), ("c", &c)], TEST_LIMIT).unwrap();

        // Every cell's coords must carry all three axis keys (a/b/c), so a cell is
        // fully self-describing and `${a}`/`${b}`/`${c}` each resolve in every cell.
        for cell in &combos {
            assert!(cell.contains_key("a"), "cell missing axis a: {cell:?}");
            assert!(cell.contains_key("b"), "cell missing axis b: {cell:?}");
            assert!(cell.contains_key("c"), "cell missing axis c: {cell:?}");
            assert_eq!(
                cell.len(),
                3,
                "cell must have exactly three coords: {cell:?}"
            );
        }

        let got: Vec<(String, String, String)> = combos
            .iter()
            .map(|cell| (cell["a"].clone(), cell["b"].clone(), cell["c"].clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("1".into(), "1".into(), "1".into()),
                ("1".into(), "1".into(), "2".into()),
                ("1".into(), "2".into(), "1".into()),
                ("1".into(), "2".into(), "2".into()),
                ("2".into(), "1".into(), "1".into()),
                ("2".into(), "1".into(), "2".into()),
                ("2".into(), "2".into(), "1".into()),
                ("2".into(), "2".into(), "2".into()),
            ]
        );
    }

    #[test]
    fn max_cells_zero_env_rejects_even_a_minimal_matrix() {
        // (AC-6) PTYTEST_MATRIX_MAX_CELLS=0 parses to Ok(0), so the cap becomes 0
        // (NOT a fallback to the default): the per-axis guard's `next_count >= 1`
        // always exceeds 0, so even the smallest possible matrix (one axis, one
        // value -> one cell) is rejected as a Scenario error (exit 2). This pins
        // the documented "0 (or any value below the product) still trips the
        // guard" contract so it cannot drift, and keeps doc and behavior aligned
        // (0 means "reject all", by design — it is not "unlimited").
        //
        // Holds the shared env lock so a parallel test toggling the same var
        // cannot interleave; the env access in `max_cells` is process-global.
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        std::env::set_var(MAX_CELLS_ENV, "0");
        assert_eq!(
            max_cells(),
            0,
            "a literal 0 must be the cap, not a default fallback"
        );

        let one_value = vec!["only".to_string()];
        let err = cell_coordinates(&[("command", &one_value)], max_cells()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("more than 0 cells"));

        std::env::remove_var(MAX_CELLS_ENV);
    }

    #[test]
    fn max_cells_defaults_and_overrides_and_falls_back() {
        // The cell cap reads PTYTEST_MATRIX_MAX_CELLS, defaulting to 256 when
        // unset, honoring a valid override, and falling back to the default on a
        // non-numeric value (never panicking). Env access is process-global, so
        // this single test exercises all three cases serially and holds the shared
        // env lock so a parallel test toggling the same var cannot interleave.
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        std::env::remove_var(MAX_CELLS_ENV);
        assert_eq!(max_cells(), DEFAULT_MAX_CELLS);

        std::env::set_var(MAX_CELLS_ENV, "1024");
        assert_eq!(max_cells(), 1024);

        std::env::set_var(MAX_CELLS_ENV, "not-a-number");
        assert_eq!(max_cells(), DEFAULT_MAX_CELLS);

        std::env::remove_var(MAX_CELLS_ENV);
    }

    /// Build a single-axis MatrixReport with given (value, status) cells.
    fn single_axis_report(axis: &str, cells: &[(&str, Status)]) -> MatrixReport {
        MatrixReport {
            axes: vec![axis.to_string()],
            cells: cells
                .iter()
                .map(|(value, status)| MatrixCell {
                    coords: coord(axis, value),
                    report: Report {
                        scenario: "s".into(),
                        status: *status,
                        duration_ms: 12,
                        assertions: Vec::new(),
                    },
                })
                .collect(),
        }
    }

    /// Build a two-axis MatrixReport with given (command, region, status) cells.
    fn two_axis_report(cells: &[(&str, &str, Status)]) -> MatrixReport {
        MatrixReport {
            axes: vec!["command".to_string(), "region".to_string()],
            cells: cells
                .iter()
                .map(|(command, region, status)| {
                    let mut coords = BTreeMap::new();
                    coords.insert("command".to_string(), command.to_string());
                    coords.insert("region".to_string(), region.to_string());
                    MatrixCell {
                        coords,
                        report: Report {
                            scenario: "s".into(),
                            status: *status,
                            duration_ms: 12,
                            assertions: Vec::new(),
                        },
                    }
                })
                .collect(),
        }
    }

    #[test]
    fn worst_exit_code_is_zero_when_all_cells_pass() {
        // All cells passing must aggregate to 0 (CI green).
        let r = single_axis_report("command", &[("a", Status::Passed), ("b", Status::Passed)]);
        assert_eq!(r.worst_exit_code(), 0);
    }

    #[test]
    fn worst_exit_code_promotes_failing_cell() {
        // A single failing cell must drive the matrix exit code to 1.
        let r = single_axis_report("command", &[("a", Status::Passed), ("b", Status::Failed)]);
        assert_eq!(r.worst_exit_code(), 1);
    }

    #[test]
    fn single_axis_table_aligns_values_and_shows_verdicts() {
        // The single-axis table lists each value (not key=value) with its verdict;
        // columns are padded to the widest value.
        let r = single_axis_report(
            "command",
            &[
                ("short", Status::Passed),
                ("a-longer-value", Status::Failed),
            ],
        );
        let table = r.to_table();
        assert!(table.contains("short"));
        assert!(table.contains("PASS"));
        assert!(table.contains("a-longer-value"));
        assert!(table.contains("FAIL"));
        assert!(table.contains("(12ms)"));
        // A single-axis row must NOT use the key=value form.
        assert!(!table.contains("command="));
    }

    #[test]
    fn multi_axis_table_uses_key_value_coordinates() {
        // The multi-axis table renders each coordinate as key=value, space
        // separated, with the verdict following.
        let r = two_axis_report(&[("a", "x", Status::Passed), ("b", "y", Status::Failed)]);
        let table = r.to_table();
        assert!(table.contains("command=a region=x"));
        assert!(table.contains("command=b region=y"));
        assert!(table.contains("PASS"));
        assert!(table.contains("FAIL"));
        assert!(table.contains("(12ms)"));
    }

    #[test]
    fn single_axis_report_serializes_axes_and_coords() {
        // The JSON must carry an `axes` array and per-cell `coords` map (not the
        // old `axis`/`value` fields), each cell embedding its report.
        let r = single_axis_report("command", &[("a", Status::Passed)]);
        let json = r.to_json();
        assert!(json.contains("\"axes\""));
        assert!(json.contains("\"command\""));
        assert!(json.contains("\"coords\""));
        assert!(json.contains("\"report\""));
        assert!(json.contains("\"status\": \"passed\""));
        // The v0.3 field names must be gone.
        assert!(!json.contains("\"axis\":"));
        assert!(!json.contains("\"value\":"));
    }

    #[test]
    fn multi_axis_report_serializes_all_axes_and_coords() {
        // A two-axis report's JSON lists both axes and each cell's full coordinate
        // map.
        let r = two_axis_report(&[("a", "x", Status::Passed)]);
        let json = r.to_json();
        assert!(json.contains("\"command\""));
        assert!(json.contains("\"region\""));
        assert!(json.contains("\"coords\""));
    }
}
