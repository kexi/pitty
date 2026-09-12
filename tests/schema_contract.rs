//! Contract gate: the published JSON schema must stay in lockstep with the
//! implementation's accepted scenario shapes.
//!
//! v1.0 freezes `schema/pitty-scenario-v1.json` as a stable, hand-written
//! contract. These tests mechanically compare the schema against the
//! implementation (the `STEP_KEYS`/`KEY_NAMES` constants, themselves pinned to
//! the deserializer by unit tests) so a step or key added on one side but not
//! the other is caught at `cargo test` time rather than shipping a schema that
//! lies about what pitty accepts.
//!
//! Key-set comparison alone is not enough: it sees the *names* on both sides
//! but nothing about the constraints around them, which is how a schema that
//! accepted a scenario pitty rejects (#40) and a schema that rejected a
//! scenario pitty runs (#32) both passed this file. So the gate also validates
//! a table of real scenario documents with a draft-07 validator and asserts the
//! schema's verdict equals pitty's own accept/reject verdict.
//!
//! The schema is embedded with `include_str!` and parsed with `serde_json`, so
//! the test reads the exact bytes that ship.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use pitty::config::{Source, Step, KEY_NAMES, STEP_KEYS};
use pitty::pty::SPLIT_MODE_NAMES;
use pitty::Scenario;
use serde_json::Value;

/// The committed schema source, embedded so the test reads exactly what ships.
const SCHEMA_JSON: &str = include_str!("../schema/pitty-scenario-v1.json");

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
/// The pattern is `^\s*(<alt>)\s*$` where `<alt>` is `name|name|...` and every
/// letter is spelled as a two-case class (`[Ee]` for `e`) — draft-07 patterns
/// are ECMA-262, which has no inline `(?i)` flag. We slice the parenthesized
/// group, split on `|`, then fold each alternative back to its canonical
/// lowercase name, expanding the one compact form `[Cc][Tt][Rr][Ll]\+[CcDdZz]`
/// into its concrete names. This reads the real alternation rather than a
/// hand-maintained copy, so the comparison is against what the schema accepts.
fn key_names_from_pattern(pattern: &str) -> BTreeSet<String> {
    let group_start = pattern.find('(').expect("pattern must have a group") + 1;
    // The group closes at the *last* `)`, just before the trailing `\s*$`.
    let group_end = pattern.rfind(')').expect("pattern must close its group");
    let alternation = &pattern[group_start..group_end];

    let mut names = BTreeSet::new();
    for alt in alternation.split('|') {
        // A trailing `[CcDdZz]` enumerates the ctrl chords; every other class
        // is a single letter in both cases, so keeping the lowercase member of
        // each class reconstructs the canonical name.
        let (prefix, chord_class) = match alt.split_once("\\+") {
            Some((prefix, chord)) => (prefix, Some(chord)),
            None => (alt, None),
        };
        let name = fold_two_case_classes(prefix);
        match chord_class {
            None => {
                names.insert(name);
            }
            Some(chord) => {
                let members = chord.trim_matches(['[', ']']);
                for c in members.chars().filter(char::is_ascii_lowercase) {
                    names.insert(format!("{name}+{c}"));
                }
            }
        }
    }
    names
}

/// Collapse a run of `[Xx]` two-case classes (and bare literals) to lowercase.
fn fold_two_case_classes(source: &str) -> String {
    let mut folded = String::new();
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        if c != '[' {
            folded.push(c.to_ascii_lowercase());
            continue;
        }
        // Inside a class, take the members and keep the lowercase one.
        let class: String = chars.by_ref().take_while(|c| *c != ']').collect();
        folded.extend(class.chars().filter(char::is_ascii_lowercase));
    }
    folded
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

// --- Document-level contract: the schema's verdict must equal pitty's --------
//
// The key-set gates above compare *names*. They cannot see constraint drift —
// a schema that accepts a document pitty rejects (or vice versa) still passes
// them, which is how #32 and #40 shipped. The table below closes that by
// running both verdicts on real scenario documents and asserting they agree.

/// How pitty judges a scenario, ignoring anything that needs a live PTY.
///
/// A Scenario error (exit 2) is raised at two moments: at parse time (unknown
/// step, bad key name, malformed value) and at step-execution time for the
/// checks the deserializer records rather than rejects — `expect_json`'s
/// one-of violation and an unrecognized `source` keyword, both carried as data
/// so the runner can report them precisely. Both moments are decidable from
/// the parsed `Scenario` alone, so this gate reaches the same verdict the
/// runner would without spawning a process.
fn pitty_accepts(yaml: &str) -> Result<(), String> {
    let scenario = Scenario::from_yaml(yaml).map_err(|e| e.to_string())?;

    for (index, step) in scenario.steps.iter().enumerate() {
        let deferred_error = match step {
            Step::ExpectJson(spec) => spec
                .invalid_reason
                .clone()
                .or_else(|| invalid_source_keyword(&spec.source)),
            Step::ExpectSemantic(spec) => invalid_source_keyword(&spec.source),
            _ => None,
        };
        if let Some(reason) = deferred_error {
            return Err(format!("steps[{index}]: {reason}"));
        }
    }
    Ok(())
}

/// Report an unrecognized `source` keyword, which the runner turns into a
/// Scenario error the moment the step executes.
fn invalid_source_keyword(source: &Source) -> Option<String> {
    match source {
        Source::Invalid(keyword) => Some(format!("unknown source keyword {keyword:?}")),
        Source::Output | Source::File(_) => None,
    }
}

#[test]
fn schema_compiles_under_a_strict_draft07_validator() {
    // The schema is only useful if the tooling SCHEMA.md points authors at can
    // load it. draft-07 specifies ECMA-262 for `pattern`, which has no inline
    // `(?i)` flag — a pattern using one is a regex syntax error in a YAML
    // language server and a metaschema violation here. Compiling the shipped
    // bytes catches that whole class before it reaches an author's editor.
    jsonschema::draft7::new(&schema()).expect("the committed schema must compile as draft-07");
}

/// Validate a scenario document against the shipped schema with a real draft-07
/// validator, so the gate tests the schema as an editor's YAML language server
/// would evaluate it — not as a hand-rolled approximation.
fn schema_accepts(yaml: &str) -> Result<(), String> {
    let document: Value =
        serde_norway::from_str(yaml).map_err(|e| format!("scenario is not valid YAML: {e}"))?;
    let validator = jsonschema::draft7::new(&schema()).expect("the committed schema must compile");
    validator
        .validate(&document)
        .map_err(|error| error.to_string())
}

/// One row of the verdict table: a scenario and whether pitty accepts it.
struct Case {
    /// What the row proves, quoted in the failure message.
    what: &'static str,
    /// The scenario document, as an author would write it.
    yaml: &'static str,
    /// pitty's verdict, which the schema's verdict must match.
    accepted: bool,
}

/// Scenarios whose schema verdict and pitty verdict must agree.
///
/// Every rejected row names the constraint it exercises; the accepted rows keep
/// the schema from over-tightening into rejecting what pitty happily runs.
const CASES: &[Case] = &[
    Case {
        what: "a minimal scenario with no steps",
        yaml: "name: ok\n",
        accepted: true,
    },
    Case {
        what: "every well-formed step kind in one scenario",
        yaml: "name: ok\nsteps:\n  - spawn: echo hi\n  - send: hi\n  - key: Ctrl+C\n  - wait: 10ms\n  - expect: {contains: hi, timeout: 1s}\n  - expect_not: {contains: bye}\n  - expect_exit: 0\n",
        accepted: true,
    },
    // #40 gap 1: an unknown step key. `minProperties`/`maxProperties` count
    // keys but never constrain their names, so this validated green until
    // `definitions.step` gained `additionalProperties: false`.
    Case {
        what: "a misspelled step name (#40)",
        yaml: "name: typo\nsteps:\n  - expct: {contains: x}\n",
        accepted: false,
    },
    Case {
        what: "a step map with two keys",
        yaml: "name: two\nsteps:\n  - {send: a, expect: {contains: b}}\n",
        accepted: false,
    },
    Case {
        what: "an unknown top-level key",
        yaml: "name: typo\nstesp: []\n",
        accepted: false,
    },
    Case {
        what: "an unrecognized key name",
        yaml: "name: badkey\nsteps:\n  - key: hyperjump\n",
        accepted: false,
    },
    // #40 gap 2: expect_json's exactly-one-of. `required: [path]` alone let a
    // zero-check and a multi-check spec validate green while pitty exits 2.
    Case {
        what: "expect_json with a single check",
        yaml: "name: json\nsteps:\n  - expect_json: {path: a.b, equals: 1}\n",
        accepted: true,
    },
    Case {
        what: "expect_json with no check (#40)",
        yaml: "name: json\nsteps:\n  - expect_json: {path: a.b}\n",
        accepted: false,
    },
    Case {
        what: "expect_json with two checks (#40)",
        yaml: "name: json\nsteps:\n  - expect_json: {path: a.b, equals: 1, contains: hi}\n",
        accepted: false,
    },
    Case {
        // The oneOf branches on key *presence*, so an explicitly written
        // `equals: null` (#30) is a check to both sides, not a zero-check spec.
        what: "expect_json asserting an explicit null (#30)",
        yaml: "name: json\nsteps:\n  - expect_json: {path: a.b, equals: null}\n",
        accepted: true,
    },
    Case {
        // v1 compatibility (round 15): 1.2.2 discarded `equals: null` while
        // deserializing, so pairing it with exactly one other check *executed*
        // that other check. COMPATIBILITY.md forbids turning a document 1.2.2 ran
        // into an error, so the runtime still accepts these two shapes — and the
        // schema has to agree, or an editor flags a scenario pitty runs.
        what: "expect_json with equals: null plus contains (v1 compatibility)",
        yaml: "name: json\nsteps:\n  - expect_json: {path: a.b, equals: null, contains: hi}\n",
        accepted: true,
    },
    Case {
        what: "expect_json with equals: null plus exists (v1 compatibility)",
        yaml: "name: json\nsteps:\n  - expect_json: {path: a.b, equals: null, exists: true}\n",
        accepted: true,
    },
    Case {
        // The carve-out is scoped to what 1.2.2 actually executed: with *two*
        // other checks it rejected the document, so both sides still reject.
        what: "expect_json with equals: null plus two other checks",
        yaml: "name: json\nsteps:\n  - expect_json: {path: a.b, equals: null, contains: hi, exists: true}\n",
        accepted: false,
    },
    Case {
        what: "expect_json with exists: false, which is not a negated check (#40)",
        yaml: "name: json\nsteps:\n  - expect_json: {path: a.b, exists: false}\n",
        accepted: false,
    },
    // #32: the deserializer trims and lowercases the `source` keyword, so the
    // schema must accept the same variants or flag scenarios pitty runs fine.
    Case {
        what: "the canonical source keyword",
        yaml: "name: src\nsteps:\n  - expect_json: {path: a, exists: true, source: output}\n",
        accepted: true,
    },
    Case {
        what: "a capitalized source keyword (#32)",
        yaml: "name: src\nsteps:\n  - expect_json: {path: a, exists: true, source: Output}\n",
        accepted: true,
    },
    Case {
        what: "an upper-cased, padded source keyword (#32)",
        yaml: "name: src\nsteps:\n  - expect_json: {path: a, exists: true, source: \"  OUTPUT  \"}\n",
        accepted: true,
    },
    Case {
        what: "an empty source keyword, which means the default output (#32)",
        yaml: "name: src\nsteps:\n  - expect_json: {path: a, exists: true, source: \"\"}\n",
        accepted: true,
    },
    Case {
        what: "a case-insensitive source keyword on expect_semantic (#32)",
        yaml: "name: src\nsteps:\n  - expect_semantic: {text: hi, similarity: 0.9, source: OUTPUT}\n",
        accepted: true,
    },
    Case {
        what: "a typo'd source keyword",
        yaml: "name: src\nsteps:\n  - expect_json: {path: a, exists: true, source: outpt}\n",
        accepted: false,
    },
    Case {
        what: "a file source mapping",
        yaml: "name: src\nsteps:\n  - expect_json: {path: a, exists: true, source: {file: report.json}}\n",
        accepted: true,
    },
    // The `spawn.split` opt-in (#34 re-landed additively): the schema must
    // accept every keyword the deserializer accepts and reject the ones it
    // rejects, in both the mapping form that carries the field and the bare
    // string form that cannot.
    Case {
        what: "spawn opting in to posix tokenization",
        yaml: "name: split\nsteps:\n  - spawn: {command: \"sh -c 'exit 3'\", split: posix}\n",
        accepted: true,
    },
    Case {
        what: "spawn naming the default tokenization explicitly",
        yaml: "name: split\nsteps:\n  - spawn: {command: echo hi, split: whitespace}\n",
        accepted: true,
    },
    Case {
        what: "a capitalized, padded split keyword",
        yaml: "name: split\nsteps:\n  - spawn: {command: echo hi, split: \"  POSIX  \"}\n",
        accepted: true,
    },
    Case {
        // Not a preference: `split` postdates 1.0, so every earlier 1.x accepts
        // this document and ignores the field. Erroring on it would tighten
        // validation on a previously valid scenario, which COMPATIBILITY.md
        // forbids within 1.x. pitty warns and uses the default instead.
        what: "a typo'd split keyword, which older pitty ignores and this one warns about",
        yaml: "name: split\nsteps:\n  - spawn: {command: echo hi, split: pisox}\n",
        accepted: true,
    },
    // A non-string `split` is the same clause one type away: 1.2.2 ignores the
    // key whatever its type, so every one of these runs there and must run here.
    Case {
        what: "a boolean split value, which older pitty ignores",
        yaml: "name: split\nsteps:\n  - spawn: {command: echo hi, split: true}\n",
        accepted: true,
    },
    Case {
        what: "an integer split value, which older pitty ignores",
        yaml: "name: split\nsteps:\n  - spawn: {command: echo hi, split: 42}\n",
        accepted: true,
    },
    Case {
        what: "a null split value, which older pitty ignores",
        yaml: "name: split\nsteps:\n  - spawn: {command: echo hi, split: null}\n",
        accepted: true,
    },
    Case {
        what: "a list split value, which older pitty ignores",
        yaml: "name: split\nsteps:\n  - spawn: {command: echo hi, split: [a, b]}\n",
        accepted: true,
    },
    Case {
        what: "a mapping split value, which older pitty ignores",
        yaml: "name: split\nsteps:\n  - spawn: {command: echo hi, split: {mode: posix}}\n",
        accepted: true,
    },
    Case {
        what: "the bare string spawn form, which carries no split and defaults to whitespace",
        yaml: "name: split\nsteps:\n  - spawn: \"echo 'hello world'\"\n",
        accepted: true,
    },
    Case {
        what: "an unsupported scenario version",
        yaml: "name: v2\nversion: 2\n",
        accepted: false,
    },
];

#[test]
fn schema_verdict_matches_pitty_verdict_on_known_documents() {
    // The gate this file was missing: for each document, the shipped schema and
    // pitty must reach the *same* accept/reject verdict. A schema that is more
    // lenient lets an editor stay silent on a scenario that exits 2 in CI (#40);
    // a schema that is stricter flags a scenario pitty runs correctly (#32).
    // Either direction is a contract break, so both are asserted here.
    let mut failures = Vec::new();

    for case in CASES {
        let pitty_verdict = pitty_accepts(case.yaml);
        assert_eq!(
            pitty_verdict.is_ok(),
            case.accepted,
            "the table claims pitty {} {}, but it {}: {pitty_verdict:?}",
            if case.accepted { "accepts" } else { "rejects" },
            case.what,
            if pitty_verdict.is_ok() {
                "accepted"
            } else {
                "rejected"
            },
        );

        let schema_verdict = schema_accepts(case.yaml);
        if schema_verdict.is_ok() != case.accepted {
            failures.push(format!(
                "- {}: pitty {}, schema {} ({})",
                case.what,
                if case.accepted { "accepts" } else { "rejects" },
                if schema_verdict.is_ok() {
                    "accepts"
                } else {
                    "rejects"
                },
                schema_verdict.err().unwrap_or_else(|| "valid".to_string()),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "schema and implementation disagree on {} document(s):\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

#[test]
fn schema_accepts_every_scenario_this_repo_ships() {
    // The table above is hand-written, so it can only test constraints someone
    // thought of. The repo's own e2e and example scenarios are the independent
    // corpus: every one of them runs under pitty, so the schema must accept all
    // of them. This is the guard against over-tightening — a constraint added
    // to close a gap like #40 that also rejects legitimate authoring shows up
    // here rather than in a user's editor.
    let roots = [
        concat!(env!("CARGO_MANIFEST_DIR"), "/e2e/scenarios"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples"),
    ];
    let mut scenarios = Vec::new();
    for root in roots {
        collect_yaml_files(Path::new(root), &mut scenarios);
    }
    assert!(
        !scenarios.is_empty(),
        "expected to find shipped scenarios under e2e/ and examples/"
    );

    let mut failures = Vec::new();
    for path in &scenarios {
        let yaml = fs::read_to_string(path).expect("a shipped scenario must be readable");
        // Only scenarios pitty itself accepts are in scope: the meta tier ships
        // deliberately broken inputs to prove pitty's error paths, and those
        // should fail the schema too.
        if pitty_accepts(&yaml).is_err() {
            continue;
        }
        if let Err(error) = schema_accepts(&yaml) {
            failures.push(format!("- {}: {error}", path.display()));
        }
    }

    assert!(
        failures.is_empty(),
        "the schema rejects {} scenario(s) this repo ships and pitty accepts:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

/// Collect every `.yaml`/`.yml` file under `dir`, recursively.
fn collect_yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_yaml_files(&path, out);
            continue;
        }
        let extension = path.extension().and_then(|e| e.to_str());
        let is_yaml = matches!(extension, Some("yaml" | "yml"));
        if is_yaml {
            out.push(path);
        }
    }
}

#[test]
fn schema_step_forbids_unknown_step_names() {
    // (#40 gap 1) Pinned structurally as well as behaviorally: `properties`
    // alone constrains nothing about unlisted keys in draft-07, so the step map
    // must carry `additionalProperties: false` for an editor to flag a typo'd
    // step name. The document table above proves the effect; this asserts the
    // mechanism, so a refactor cannot drop the keyword and pass by accident.
    let schema = schema();
    assert_eq!(
        schema["definitions"]["step"]["additionalProperties"],
        Value::Bool(false),
        "definitions.step must forbid unknown step names (#40)"
    );
}

#[test]
fn schema_expect_json_encodes_the_one_of_constraint() {
    // (#40 gap 2) The "exactly one of equals/contains/exists" rule the
    // deserializer enforces must be encoded, not merely described in prose.
    //
    // The encoding gained a second branch in round 15. The rule is no longer a
    // bare `oneOf`: a null `equals` alongside exactly one other check is
    // *accepted*, because pitty 1.2.2 executed those documents and
    // COMPATIBILITY.md forbids turning them into errors. So the shape is an
    // `anyOf` of [the ordinary exactly-one `oneOf`, the null-equals carve-out],
    // and this test pins both halves rather than the old single branch.
    let schema = schema();
    let expect_json = &schema["definitions"]["step"]["properties"]["expect_json"];
    let alternatives = expect_json["anyOf"]
        .as_array()
        .expect("expect_json must encode its one-of as an anyOf of two branches");
    assert_eq!(
        alternatives.len(),
        2,
        "expected [ordinary one-of, null-equals carve-out]"
    );

    // Branch 1: the ordinary rule, still a oneOf over exactly the three checks.
    let ordinary: BTreeSet<&str> = alternatives[0]["oneOf"]
        .as_array()
        .expect("the first branch must be the exactly-one-of")
        .iter()
        .filter_map(|branch| branch["required"].as_array())
        .flat_map(|names| names.iter().filter_map(Value::as_str))
        .collect();
    assert_eq!(
        ordinary,
        BTreeSet::from(["equals", "contains", "exists"]),
        "the ordinary branch must still branch on exactly the three check fields"
    );

    // Branch 2: the carve-out is scoped — a *null* equals, plus exactly one of
    // contains/exists. It must not admit the two-other-checks shape, which 1.2.2
    // rejected and both sides still reject.
    let carve_out = &alternatives[1];
    assert_eq!(
        carve_out["properties"]["equals"]["type"], "null",
        "the carve-out must apply only to an explicitly null equals"
    );
    let paired: BTreeSet<&str> = carve_out["oneOf"]
        .as_array()
        .expect("the carve-out must pair the null with exactly one other check")
        .iter()
        .filter_map(|branch| branch["required"].as_array())
        .flat_map(|names| names.iter().filter_map(Value::as_str))
        .collect();
    assert_eq!(
        paired,
        BTreeSet::from(["contains", "exists"]),
        "the carve-out must cover the two shapes 1.2.2 executed, and no others"
    );
}

#[test]
fn schema_does_not_constrain_split_to_the_known_keywords() {
    // The inverse of the `key` mirror gate, and deliberately so.
    //
    // `key` and `source` existed in 1.0, so an unknown value for them is an
    // error in every 1.x and the schema may pin them to an alternation. `split`
    // did NOT exist in 1.0: a pitty predating it accepts `split: <anything>`
    // and ignores it, so this pitty must accept it too (COMPATIBILITY.md
    // forbids tightening validation on a previously valid scenario). A
    // `pattern` here would make an editor flag a document pitty runs — exactly
    // the drift this file exists to forbid, in the #32 direction.
    //
    // `type` is included in that prohibition, not just `pattern`/`enum`: older
    // pitty ignores the key whatever its *type*, so `split: 42` is a document
    // pitty runs, and `"type": "string"` would flag it.
    //
    // The known keywords are still advertised, via `examples`, which documents
    // without constraining.
    let schema = schema();
    let split = &schema["definitions"]["splitMode"];
    assert!(
        split["pattern"].is_null() && split["enum"].is_null() && split["type"].is_null(),
        "splitMode must not constrain its value set *or its type*: {split}"
    );
    let examples: BTreeSet<String> = split["examples"]
        .as_array()
        .expect("splitMode should advertise its keywords via examples")
        .iter()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();
    let impl_names: BTreeSet<String> = SPLIT_MODE_NAMES.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        examples, impl_names,
        "splitMode's examples should name exactly the keywords this build recognizes"
    );
}

#[test]
fn schema_documents_the_legacy_default_for_split() {
    // The v1 contract (COMPATIBILITY.md) forbids changing the default of an
    // existing behavior, so `split` must document `whitespace` as its default.
    // An editor showing `posix` here would tell authors the opposite of what
    // pitty does, which is the drift this whole file exists to forbid.
    let schema = schema();
    assert_eq!(
        schema["definitions"]["splitMode"]["default"],
        Value::String("whitespace".to_string()),
        "splitMode's documented default must stay the legacy whitespace rule"
    );
}
