//! Contract gate: the published JSON schema must stay in lockstep with the
//! implementation's accepted scenario shapes.
//!
//! v1.0 freezes `schema/ptytest-scenario-v1.json` as a stable, hand-written
//! contract. These tests mechanically compare the schema against the
//! implementation (the `STEP_KEYS`/`KEY_NAMES` constants, themselves pinned to
//! the deserializer by unit tests) so a step or key added on one side but not
//! the other is caught at `cargo test` time rather than shipping a schema that
//! lies about what ptytest accepts.
//!
//! The schema is embedded with `include_str!` and parsed with `serde_json`, so
//! the test reads the exact bytes that ship.

use std::collections::BTreeSet;

use ptytest::config::{KEY_NAMES, STEP_KEYS};
use serde_json::Value;

/// The committed schema source, embedded so the test reads exactly what ships.
const SCHEMA_JSON: &str = include_str!("../schema/ptytest-scenario-v1.json");

/// Parse the embedded schema once.
fn schema() -> Value {
    serde_json::from_str(SCHEMA_JSON).expect("the committed schema must be valid JSON")
}

#[test]
fn schema_step_properties_match_implementation_step_keys() {
    // (G-8) The schema's `definitions.step.properties` keys must equal the set
    // of step keys the deserializer accepts (STEP_KEYS). A mismatch means a step
    // was added/renamed/removed on one side only — exactly the drift this gate
    // exists to forbid for the stable v1 schema.
    let schema = schema();
    let props = schema["definitions"]["step"]["properties"]
        .as_object()
        .expect("schema step must have a properties object");
    let schema_keys: BTreeSet<&str> = props.keys().map(String::as_str).collect();
    let impl_keys: BTreeSet<&str> = STEP_KEYS.iter().copied().collect();

    assert_eq!(
        schema_keys, impl_keys,
        "schema step.properties and STEP_KEYS diverged:\n  only in schema: {:?}\n  only in impl:   {:?}",
        schema_keys.difference(&impl_keys).collect::<Vec<_>>(),
        impl_keys.difference(&schema_keys).collect::<Vec<_>>(),
    );
}

#[test]
fn schema_key_pattern_covers_exactly_the_resolved_key_names() {
    // (G-8 bonus) The schema constrains `key` with a regex alternation. Its set
    // of alternatives must equal the names Key::resolve accepts (KEY_NAMES), so
    // the schema neither advertises a key the runtime rejects nor omits one it
    // accepts. We extract the alternation rather than re-run the regex because we
    // are checking the *name set*, not match behavior.
    let schema = schema();
    let pattern = schema["definitions"]["step"]["properties"]["key"]["pattern"]
        .as_str()
        .expect("schema key must carry a string pattern");

    let schema_names = key_names_from_pattern(pattern);
    let impl_names: BTreeSet<String> = KEY_NAMES.iter().map(|s| s.to_string()).collect();

    assert_eq!(
        schema_names, impl_names,
        "schema key pattern and KEY_NAMES diverged (pattern was {pattern:?})"
    );
}

/// Extract the literal key names from the schema's `key` pattern alternation.
///
/// The pattern is `(?i)^\s*(<alt>)\s*$` where `<alt>` is `name|name|...`. We
/// slice the parenthesized group and split on `|`, then expand the one compact
/// alternative `ctrl\+[cdz]` into its concrete names. This mirrors exactly the
/// names the schema accepts so the comparison is against the real alternation,
/// not a hand-maintained copy.
fn key_names_from_pattern(pattern: &str) -> BTreeSet<String> {
    let open = pattern.find('(').expect("pattern must have a group");
    // The capturing group is the *last* parenthesized group before `)\s*$`.
    let group_start = pattern.rfind('(').unwrap_or(open) + 1;
    let group_end = pattern.rfind(')').expect("pattern must close its group");
    let alternation = &pattern[group_start..group_end];

    let mut names = BTreeSet::new();
    for alt in alternation.split('|') {
        let alt = alt.replace("\\+", "+");
        if alt == "ctrl+[cdz]" {
            for c in ['c', 'd', 'z'] {
                names.insert(format!("ctrl+{c}"));
            }
        } else {
            names.insert(alt);
        }
    }
    names
}

#[test]
fn schema_top_level_is_strict_but_specs_are_lenient() {
    // The top-level object denies unknown properties (catches typos like
    // `stesp:`), mirroring `Scenario`'s `deny_unknown_fields`. expect_not is the
    // one spec deliberately strict (additionalProperties: false) so an editor
    // flags its no-longer-supported `timeout` (S-1).
    let schema = schema();
    assert_eq!(
        schema["additionalProperties"],
        Value::Bool(false),
        "top-level additionalProperties must be false (mirrors deny_unknown_fields)"
    );
    assert_eq!(
        schema["definitions"]["expectNotSpec"]["additionalProperties"],
        Value::Bool(false),
        "expectNotSpec must forbid extra properties so editors flag a stray timeout (S-1)"
    );
}
