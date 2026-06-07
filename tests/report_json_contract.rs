//! Contract gate: the output report JSON field sets are frozen at v1.0.
//!
//! COMPATIBILITY.md declares `Report`, `MatrixReport`, and `BenchReport` a
//! stable output contract: removing a field, renaming it, or changing its type
//! is a breaking (major) change; adding one is minor. These tests pin the
//! *exact* top-level key set of each serialized report (and the nested
//! assertion / cell / coords / stats objects), so an accidental rename or
//! removal fails CI instead of silently breaking downstream consumers.
//!
//! Why assert the whole key set (not just presence): a removal is as breaking as
//! a rename, and only an exact-set check catches a dropped field. A purely
//! additive (minor) change updates this list deliberately, documenting the
//! contract bump in the diff.

use std::collections::BTreeSet;

use ptytest::report::{Report, Status};
use serde_json::Value;

/// The set of top-level object keys in a JSON value.
fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("expected a JSON object")
        .keys()
        .cloned()
        .collect()
}

/// A set literal helper.
fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn report_json_field_set_is_frozen() {
    use ptytest::assert::AssertionResult;

    let report = Report {
        scenario: "s".into(),
        status: Status::Failed,
        duration_ms: 7,
        assertions: vec![AssertionResult::fail("expect: x", "saw y")],
    };
    let json: Value = serde_json::from_str(&report.to_json()).unwrap();

    assert_eq!(
        keys(&json),
        set(&["scenario", "status", "duration_ms", "assertions"]),
        "Report top-level field set changed (breaking unless purely additive)"
    );
    // status must serialize lowercase, the documented consumer-facing form.
    assert_eq!(json["status"], Value::String("failed".into()));
    // Each assertion object's field set is part of the same contract.
    assert_eq!(
        keys(&json["assertions"][0]),
        set(&["step", "passed", "message"]),
        "AssertionResult field set changed"
    );
}

#[test]
fn matrix_report_json_field_set_is_frozen() {
    use ptytest::matrix::{MatrixCell, MatrixReport};
    use std::collections::BTreeMap;

    let mut coords = BTreeMap::new();
    coords.insert("command".to_string(), "echo".to_string());
    let report = MatrixReport {
        axes: vec!["command".to_string()],
        cells: vec![MatrixCell {
            coords,
            report: Report {
                scenario: "m".into(),
                status: Status::Passed,
                duration_ms: 3,
                assertions: Vec::new(),
            },
        }],
    };
    let json: Value = serde_json::from_str(&report.to_json()).unwrap();

    assert_eq!(
        keys(&json),
        set(&["axes", "cells"]),
        "MatrixReport top-level field set changed"
    );
    assert_eq!(
        keys(&json["cells"][0]),
        set(&["coords", "report"]),
        "MatrixCell field set changed"
    );
    // A nested cell report keeps the same Report contract.
    assert_eq!(
        keys(&json["cells"][0]["report"]),
        set(&["scenario", "status", "duration_ms", "assertions"]),
        "nested cell Report field set changed"
    );
}

#[test]
fn bench_report_json_field_set_is_frozen() {
    use ptytest::bench::{BenchReport, Stats};

    let report = BenchReport {
        scenario: "b".into(),
        runs: 3,
        warmup: 1,
        pass_count: 3,
        durations: vec![10, 20, 30],
        stats: Stats {
            min: 10,
            max: 30,
            mean: 20,
            median: 20,
            p95: 30,
            stddev: 8,
        },
    };
    let json: Value = serde_json::from_str(&report.to_json()).unwrap();

    assert_eq!(
        keys(&json),
        set(&[
            "scenario",
            "runs",
            "warmup",
            "pass_count",
            "durations",
            "stats"
        ]),
        "BenchReport top-level field set changed"
    );
    assert_eq!(
        keys(&json["stats"]),
        set(&["min", "max", "mean", "median", "p95", "stddev"]),
        "Stats field set changed"
    );
}
