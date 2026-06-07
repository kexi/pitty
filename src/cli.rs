//! CLI definition and dispatch.
//!
//! Five subcommands — `init`, `run`, `list`, `matrix`, `bench` — built with
//! clap's derive API.
//! `dispatch` returns the process exit code as a `u8` so `main` can convert it
//! to an `ExitCode` at the boundary.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::config::Scenario;
use crate::error::{severity, PittyError};
use crate::github::{self, github_enabled};
use crate::runner::{run_scenario, RunOptions};

/// Embedded scaffold scenario, copied by `init`.
const HELLO_SCENARIO: &str = include_str!("../assets/scenarios/hello.yaml");
/// Embedded top-level config, copied by `init`.
const DEFAULT_CONFIG: &str = include_str!("../assets/pitty.yaml");

/// pitty command-line interface.
#[derive(Debug, Parser)]
#[command(name = "pitty", version, about = "PTY-based CLI testing framework")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scaffold `pitty.yaml` and `scenarios/` in the current directory.
    Init,
    /// Run a scenario file or every scenario in a directory.
    Run {
        /// Path to a `.yaml`/`.yml` file or a directory of scenarios.
        path: PathBuf,
        /// Record or refresh `expect_snapshot` files instead of failing on an
        /// absent or mismatched snapshot. Also enabled by setting the
        /// `PITTY_UPDATE_SNAPSHOTS` environment variable.
        #[arg(long)]
        update: bool,
        /// Emit a GitHub Actions step summary and failure annotations. Auto-on
        /// when `GITHUB_ACTIONS=true`; this flag forces it on elsewhere.
        #[arg(long)]
        github: bool,
    },
    /// List scenario names found in a directory (default `scenarios/`).
    List {
        /// Directory to scan; defaults to `scenarios/`.
        dir: Option<PathBuf>,
    },
    /// Run a matrix scenario once per cell of its axes' Cartesian product.
    Matrix {
        /// Path to a scenario `.yaml`/`.yml` file carrying a `matrix:` section
        /// (one or more axes; cells are the product of all axes).
        file: PathBuf,
        /// Emit a machine-readable `MatrixReport` as JSON instead of the table.
        #[arg(long)]
        json: bool,
        /// Run every cell but always exit 0 (do not fail CI on a failing cell).
        #[arg(long)]
        no_fail: bool,
        /// Emit a GitHub Actions step summary and per-cell failure annotations.
        /// Auto-on when `GITHUB_ACTIONS=true`.
        #[arg(long)]
        github: bool,
    },
    /// Repeat a scenario to measure duration statistics and detect flakiness.
    Bench {
        /// Path to a scenario `.yaml`/`.yml` file.
        file: PathBuf,
        /// Number of measured runs (warmup excluded). Defaults to 10.
        #[arg(long, default_value_t = 10)]
        runs: usize,
        /// Number of discarded warmup runs. Defaults to 0.
        #[arg(long, default_value_t = 0)]
        warmup: usize,
        /// Emit a machine-readable `BenchReport` as JSON instead of the summary.
        #[arg(long)]
        json: bool,
        /// Emit a GitHub Actions step summary and a flakiness warning. Auto-on
        /// when `GITHUB_ACTIONS=true`.
        #[arg(long)]
        github: bool,
    },
}

/// Parse arguments and dispatch, returning the process exit code.
pub fn dispatch() -> u8 {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => cmd_init(),
        Command::Run {
            path,
            update,
            github,
        } => {
            // The env var is an OR with the flag so CI can globally opt in to
            // updating snapshots without editing every invocation.
            let update = update || env_update_enabled();
            // The semantic backend defaults to the lexical one here. A future
            // `--features semantic-embeddings` build would swap this single
            // construction site (e.g. behind a flag) without touching the
            // runner, which only calls through the `SemanticBackend` trait.
            let options = RunOptions {
                update,
                ..RunOptions::default()
            };
            // GitHub output mirrors the --update pattern: flag OR env detection.
            let github = github_enabled(github);
            cmd_run(&path, &options, github)
        }
        Command::List { dir } => cmd_list(dir.as_deref()),
        Command::Matrix {
            file,
            json,
            no_fail,
            github,
        } => cmd_matrix(&file, json, no_fail, github_enabled(github)),
        Command::Bench {
            file,
            runs,
            warmup,
            json,
            github,
        } => cmd_bench(&file, runs, warmup, json, github_enabled(github)),
    }
}

/// Whether `PITTY_UPDATE_SNAPSHOTS` is set to a truthy value.
///
/// Accepts `1`, `true`, or `yes`, case-insensitively (the value is trimmed and
/// lowercased first), so `TRUE`/`Yes`/` true ` all enable updating. Any other
/// value (or an unset var) is false.
fn env_update_enabled() -> bool {
    match std::env::var("PITTY_UPDATE_SNAPSHOTS") {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => false,
    }
}

/// `pitty init`: write the scaffold without clobbering existing files.
///
/// Existing files are left untouched (with a warning) rather than overwritten,
/// so re-running `init` in a populated project is safe.
fn cmd_init() -> u8 {
    let config_path = Path::new("pitty.yaml");
    write_if_absent(config_path, DEFAULT_CONFIG);

    let scenarios_dir = Path::new("scenarios");
    if let Err(e) = std::fs::create_dir_all(scenarios_dir) {
        eprintln!("error: cannot create scenarios/: {e}");
        return 3;
    }
    write_if_absent(&scenarios_dir.join("hello.yaml"), HELLO_SCENARIO);

    println!("Initialized pitty project (pitty.yaml, scenarios/hello.yaml).");
    0
}

/// Write `contents` to `path` only if it does not already exist.
fn write_if_absent(path: &Path, contents: &str) {
    if path.exists() {
        eprintln!(
            "warning: {} already exists; leaving it unchanged",
            path.display()
        );
        return;
    }
    match std::fs::write(path, contents) {
        Ok(()) => println!("created {}", path.display()),
        Err(e) => eprintln!("error: cannot write {}: {e}", path.display()),
    }
}

/// `pitty run <path>`: run one file or all scenarios under a directory.
///
/// Multiple scenarios run sequentially in name order; the final exit code is
/// the most severe outcome across them (process > scenario > assertion >
/// success).
fn cmd_run(path: &Path, options: &RunOptions, github: bool) -> u8 {
    let files = match collect_scenarios(path) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("error: {e}");
            return e.exit_code();
        }
    };

    if files.is_empty() {
        eprintln!("error: no scenarios found at {}", path.display());
        return 2;
    }

    // Every scenario runs (no fail-fast across files); the process exit code is
    // the most severe outcome observed.
    let codes = files.iter().map(|file| run_one(file, options, github));
    aggregate_exit_codes(codes)
}

/// Reduce a sequence of per-scenario exit codes to the single most severe code.
///
/// Severity order (high to low): process (3) > scenario (2) > assertion (1) >
/// success (0). Pulled out as a pure function so the aggregation can be tested
/// without spawning real scenarios. An empty sequence yields `0`; callers that
/// must treat "no scenarios" as an error handle that before reaching here.
pub(crate) fn aggregate_exit_codes(codes: impl IntoIterator<Item = u8>) -> u8 {
    let mut worst: u8 = 0;
    for code in codes {
        if severity(code) > severity(worst) {
            worst = code;
        }
    }
    worst
}

/// Run a single scenario file and return its exit code, printing the report.
fn run_one(file: &Path, options: &RunOptions, github: bool) -> u8 {
    let scenario = match Scenario::from_path(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error in {}: {e}", file.display());
            return e.exit_code();
        }
    };

    // A scenario carrying a matrix section is not silently single-run: that
    // would execute exactly one arbitrary cell (whichever the `${axis}` resolves
    // to) and hide the other cells. Refuse with guidance to the right command.
    if scenario.has_matrix() {
        eprintln!(
            "error in {}: scenario has a matrix section; use `pitty matrix` to run it",
            file.display()
        );
        return 2;
    }

    // Relative paths in the scenario resolve against its own directory.
    let base_dir = file.parent().unwrap_or_else(|| Path::new("."));

    match run_scenario(&scenario, base_dir, options) {
        Ok(report) => {
            println!("{}", report.to_json());
            // GitHub output is a side effect: emit the summary/annotations from
            // the already-masked report, then let the exit code be the verdict.
            // secret_values() reproduces the workspace masking set without
            // building a workspace, and the github module masks again as defense
            // in depth so no secret reaches the summary/annotation/log.
            if github {
                github::report_outputs(&report, &scenario.secret_values());
            }
            // Single Status -> exit-code table (see report::status_exit_code):
            // Passed 0 / Failed 1 (an assertion did not hold). Hard faults never
            // reach this arm — run_scenario returns Err for them, handled below
            // via PittyError::exit_code (scenario 2 / process 3).
            crate::report::status_exit_code(report.status)
        }
        Err(e) => {
            eprintln!("error in {}: {e}", file.display());
            e.exit_code()
        }
    }
}

/// Resolve `path` into the ordered list of scenario files to run.
fn collect_scenarios(path: &Path) -> Result<Vec<PathBuf>, PittyError> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if path.is_dir() {
        return Ok(list_yaml_files(path));
    }
    Err(PittyError::Scenario(format!(
        "path does not exist: {}",
        path.display()
    )))
}

/// Collect `*.yaml`/`*.yml` files in `dir`, sorted by name for deterministic
/// run order.
fn list_yaml_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            // Require a regular file: a directory named `foo.yaml` has the
            // extension but is not a scenario, and `Scenario::from_path` would
            // fail to read it. Filtering here keeps run/list to actual files.
            .filter(|p| p.is_file() && is_yaml(p))
            .collect(),
        Err(_) => Vec::new(),
    };
    files.sort();
    files
}

/// Whether a path has a `.yaml` or `.yml` extension.
fn is_yaml(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    )
}

/// `pitty list`: print scenario names found under `dir` (default
/// `scenarios/`).
///
/// Each file is parsed only enough to read its `name`; an unparseable file is
/// reported inline but does not abort listing the rest.
fn cmd_list(dir: Option<&Path>) -> u8 {
    let dir = dir.unwrap_or_else(|| Path::new("scenarios"));
    let files = list_yaml_files(dir);
    if files.is_empty() {
        eprintln!("no scenarios found in {}", dir.display());
        return 0;
    }
    for file in files {
        match Scenario::from_path(&file) {
            Ok(s) => println!("{:<24} {}", s.name, file.display()),
            Err(e) => eprintln!("{:<24} (parse error: {e})", file.display()),
        }
    }
    0
}

/// `pitty matrix <file>`: run a matrix scenario once per cell of its axes'
/// Cartesian product.
///
/// Prints the aligned PASS/FAIL table (or JSON with `--json`) and returns the
/// worst per-cell exit code, so one failing cell fails CI — unless `--no-fail`
/// forces exit 0 after walking every cell.
fn cmd_matrix(file: &Path, json: bool, no_fail: bool, github: bool) -> u8 {
    let scenario = match Scenario::from_path(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error in {}: {e}", file.display());
            return e.exit_code();
        }
    };

    let base_dir = file.parent().unwrap_or_else(|| Path::new("."));

    // Matrix runs always compare against existing snapshots; they never record
    // or refresh them (RunOptions::default has update=false, and matrix has no
    // --update flag). Why not allow updating: every cell shares the same
    // snapshot path, so the last cell would clobber whatever the earlier cells
    // recorded — a write race that makes the "golden" file meaningless. A cell
    // whose snapshot is absent therefore fails (`not recorded; rerun with
    // --update`); record snapshots with `pitty run --update` first, then gate
    // with `pitty matrix`.
    match crate::matrix::run_matrix(&scenario, base_dir, &RunOptions::default()) {
        Ok(report) => {
            if json {
                println!("{}", report.to_json());
            } else {
                print!("{}", report.to_table());
            }
            if github {
                github::matrix_outputs(&report, &scenario.secret_values());
            }
            // --no-fail walks every cell but never fails the process, for the
            // "observe all impls" use case where a red cell is informational.
            //
            // Why this only suppresses assertion failures, not hard faults: we
            // are on the `Ok(report)` arm, which `run_matrix` reaches only when
            // every cell completed (each cell's status is Passed/Failed). A hard
            // fault (spawn failure, scenario error) returns `Err` from
            // `run_matrix` and aborts the remaining cells, taking the `Err` arm
            // below — so --no-fail can never reach it and a fault still exits
            // 2/3. Suppressing a fault would hide a broken harness, not just a red
            // assertion, which is not what "observe all impls" asks for.
            if no_fail {
                0
            } else {
                report.worst_exit_code()
            }
        }
        Err(e) => {
            eprintln!("error in {}: {e}", file.display());
            e.exit_code()
        }
    }
}

/// `pitty bench <file>`: repeat a scenario and report duration statistics.
///
/// Prints the human summary (or JSON with `--json`) and returns 0 only when
/// every measured run passed; any assertion failure yields 1, and a hard fault
/// yields its severity class (scenario 2 / process 3).
fn cmd_bench(file: &Path, runs: usize, warmup: usize, json: bool, github: bool) -> u8 {
    if runs == 0 {
        eprintln!("error: --runs must be at least 1");
        return 2;
    }

    let scenario = match Scenario::from_path(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error in {}: {e}", file.display());
            return e.exit_code();
        }
    };

    // A matrix scenario must not be benched. `run_bench` calls `run_scenario`
    // directly, which never injects the matrix axis values (only `run_matrix`
    // does, via `inject_cell_values`). So every run would leave `${axis}`
    // unexpanded and identical — N copies of a meaningless result with the
    // matrix silently ignored, and the matrix's own validation (empty/
    // unreferenced/secret-collision) skipped. Reject it like `run` does.
    if scenario.has_matrix() {
        eprintln!(
            "error in {}: scenario has a matrix section; use `pitty matrix` to run it",
            file.display()
        );
        return 2;
    }

    let base_dir = file.parent().unwrap_or_else(|| Path::new("."));

    // Bench runs always compare against existing snapshots; they never record or
    // refresh them (RunOptions::default has update=false, and bench has no
    // --update flag). Why not allow updating: the scenario runs N times, so
    // every run after the first would re-record the same snapshot from a slightly
    // different output, and the recorded "golden" would just be whichever run
    // happened to write last — noise, not a baseline. A run whose snapshot is
    // absent therefore fails; record with `pitty run --update` first.
    match crate::bench::run_bench(&scenario, base_dir, &RunOptions::default(), runs, warmup) {
        Ok(report) => {
            if json {
                println!("{}", report.to_json());
            } else {
                println!("{}", report.to_summary());
            }
            if github {
                github::bench_outputs(&report, &scenario.secret_values());
            }
            crate::bench::bench_exit_code(&report)
        }
        Err(e) => {
            // Mirror matrix's error path: a hard fault maps straight through
            // `PittyError::exit_code` (scenario 2 / process 3), so the two
            // orchestration commands share one exit-code conversion path.
            eprintln!("error in {}: {e}", file.display());
            e.exit_code()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn aggregate_picks_most_severe_with_assertion_mix() {
        // success + assertion failure + scenario error must aggregate to the
        // scenario class (2), the most severe of the three.
        assert_eq!(aggregate_exit_codes([0, 1, 2]), 2);
    }

    #[test]
    fn aggregate_promotes_process_error() {
        // A process error (3) anywhere dominates lesser outcomes.
        assert_eq!(aggregate_exit_codes([1, 3, 2, 0]), 3);
    }

    #[test]
    fn aggregate_of_all_success_is_zero() {
        // All-success aggregates to 0; an empty sequence is also 0.
        assert_eq!(aggregate_exit_codes([0, 0]), 0);
        assert_eq!(aggregate_exit_codes([]), 0);
    }

    /// Write a scenario file under `dir/name` and return its path.
    fn write_scenario(dir: &Path, name: &str, yaml: &str) {
        fs::write(dir.join(name), yaml).unwrap();
    }

    /// A file-only scenario asserting `target` exists (passes iff present).
    fn exists_scenario(target: &str) -> String {
        format!("name: {target}\nsteps:\n  - expect_file_exists:\n      path: {target}\n")
    }

    #[test]
    fn cmd_run_directory_runs_all_files_worst_assertion() {
        // success / assertion-failure / success across three files must all run
        // and aggregate to worst = 1 (the failing middle scenario).
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::write(d.join("present.txt"), b"x").unwrap();
        write_scenario(d, "a.yaml", &exists_scenario("present.txt"));
        write_scenario(d, "b.yaml", &exists_scenario("missing.txt")); // fails: absent
        write_scenario(d, "c.yaml", &exists_scenario("present.txt"));
        assert_eq!(cmd_run(d, &RunOptions::default(), false), 1);
    }

    #[test]
    fn cmd_run_directory_continues_past_scenario_error_worst_scenario() {
        // A middle scenario error (send before spawn -> exit 2) must not stop
        // later files; the worst across all is the scenario class (2).
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::write(d.join("present.txt"), b"x").unwrap();
        write_scenario(d, "a.yaml", &exists_scenario("present.txt"));
        write_scenario(d, "b.yaml", "name: bad\nsteps:\n  - send: hi\n"); // exit 2
        write_scenario(d, "c.yaml", &exists_scenario("present.txt"));
        assert_eq!(cmd_run(d, &RunOptions::default(), false), 2);
    }

    #[test]
    fn list_yaml_files_ignores_non_yaml_and_subdirs_and_sorts() {
        // Only top-level *.yaml/*.yml count; .txt and subdirectories are
        // ignored, and the result is sorted by name for deterministic order.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::write(d.join("b.yaml"), b"name: b\nsteps: []\n").unwrap();
        fs::write(d.join("a.yml"), b"name: a\nsteps: []\n").unwrap();
        fs::write(d.join("notes.txt"), b"ignore me").unwrap();
        fs::create_dir(d.join("sub.yaml")).unwrap(); // a directory, not a file
        fs::write(d.join("sub.yaml/nested.yaml"), b"name: n\nsteps: []\n").unwrap();

        let files = list_yaml_files(d);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.yml", "b.yaml"]);
    }

    #[test]
    fn collect_scenarios_empty_dir_is_empty_so_run_exits_two() {
        // An empty directory yields no scenarios; cmd_run treats "no scenarios"
        // as a scenario-class error (exit 2).
        let dir = tempfile::tempdir().unwrap();
        assert!(collect_scenarios(dir.path()).unwrap().is_empty());
        assert_eq!(cmd_run(dir.path(), &RunOptions::default(), false), 2);
    }

    #[test]
    fn run_refuses_matrix_scenario_with_guidance() {
        // `pitty run` on a matrix scenario must not silently single-run; it
        // returns the scenario class (2) and points to `pitty matrix`.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.yaml");
        fs::write(
            &file,
            "name: m\nmatrix:\n  command: [a]\nsteps:\n  - spawn: \"${command}\"\n",
        )
        .unwrap();
        assert_eq!(run_one(&file, &RunOptions::default(), false), 2);
    }

    #[test]
    fn bench_refuses_matrix_scenario_with_guidance() {
        // `pitty bench` on a matrix scenario must reject it (exit 2): run_bench
        // would call run_scenario N times without injecting the axis values, so
        // every run would be identical with `${command}` unexpanded. Like `run`,
        // it points the user at `pitty matrix`.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.yaml");
        fs::write(
            &file,
            "name: m\nmatrix:\n  command: [a]\nsteps:\n  - spawn: \"${command}\"\n",
        )
        .unwrap();
        assert_eq!(cmd_bench(&file, 3, 0, false, false), 2);
    }

    #[test]
    fn matrix_command_rejects_file_without_matrix_section() {
        // `pitty matrix` on a plain scenario (no matrix:) must error (exit 2)
        // rather than run it as a single cell.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.yaml");
        fs::write(&file, "name: plain\nsteps: []\n").unwrap();
        assert_eq!(cmd_matrix(&file, false, false, false), 2);
    }

    #[test]
    fn matrix_command_rejects_oversized_product_before_spawning() {
        // A product exceeding the cell cap must be a scenario error (exit 2)
        // raised before any cell spawns. With the cap lowered to 1 via the env
        // override, a two-cell single axis already trips it, so this never
        // actually spawns a process.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.yaml");
        fs::write(
            &file,
            "name: m\nmatrix:\n  command: [a, b]\nsteps:\n  - spawn: \"${command}\"\n",
        )
        .unwrap();
        // Hold the shared env lock so this set/remove cannot interleave with the
        // matrix module's max_cells test, which reads the same var.
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        std::env::set_var("PITTY_MATRIX_MAX_CELLS", "1");
        let code = cmd_matrix(&file, false, false, false);
        std::env::remove_var("PITTY_MATRIX_MAX_CELLS");
        assert_eq!(code, 2);
    }

    #[test]
    fn bench_rejects_zero_runs() {
        // --runs 0 has no measured runs to characterize; reject as scenario
        // misuse (exit 2) before spawning anything.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.yaml");
        fs::write(&file, "name: s\nsteps: []\n").unwrap();
        assert_eq!(cmd_bench(&file, 0, 0, false, false), 2);
    }
}
