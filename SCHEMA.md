# ptytest scenario format (stable, v1)

This document specifies the **stable scenario input format** as of ptytest
`1.0.0`. A scenario is a single YAML document. Unless noted otherwise, every
key shown is optional with the stated default.

A machine-readable JSON Schema for editor autocompletion and validation lives
at [`schema/ptytest-scenario-v1.json`](schema/ptytest-scenario-v1.json). Point
your YAML language server at it with a modeline at the top of a scenario file:

```yaml
# yaml-language-server: $schema=./schema/ptytest-scenario-v1.json
```

(Use a relative path that resolves to the schema in this repo, or a checked-in
copy. ptytest does not publish the schema to an external URL.)

## Top-level keys

A scenario document accepts exactly these seven keys. **Any other top-level key
is a Scenario error** (exit code 2): ptytest applies `deny_unknown_fields` at
the top level so a typo such as `stesp:` (for `steps:`) fails loudly instead of
leaving an empty step list that passes vacuously.

| Key         | Type                          | Default | Meaning |
|-------------|-------------------------------|---------|---------|
| `version`   | integer                       | `1`     | Scenario format version (see below). |
| `name`      | string (**required**)         | —       | Human-readable name, shown by `list` and in reports/logs. |
| `variables` | map of name → var spec        | `{}`    | Values for `${var}` expansion; a variable may be marked secret. |
| `env`       | map of string → string        | `{}`    | Environment variables injected into every spawned process. |
| `workspace` | workspace spec                | `{cwd: ".", temp: false}` | Where commands run. |
| `matrix`    | map of axis → list of strings | `{}`    | Matrix axes; cells are the Cartesian product (run with `ptytest matrix`). |
| `steps`     | list of steps                 | `[]`    | Ordered steps to execute. |

### `version`

`version` is an optional integer. When omitted it defaults to `1`. This build
supports **version 1 only**. A scenario declaring any other version (e.g.
`version: 2`) is rejected with a Scenario error:

```
unsupported scenario version 2; this ptytest supports version 1 (update ptytest for newer scenarios)
```

This is deliberate: silently parsing a newer scenario could drop steps or field
semantics the new version introduced and report a false pass. Update ptytest to
run a newer scenario.

### `variables`

Each entry is either a bare string (a plain value) or a mapping with a secret
flag:

```yaml
variables:
  username: test-user          # plain value
  token:                       # secret value
    value: secret-token
    secret: true
```

A secret variable's literal value is masked (`***`) in stdout reports, logs,
error messages, and GitHub Actions step summaries/annotations. A matrix axis
must not share a name with a secret variable (that would unmask it).

### `env`

A string → string map injected into the environment of every spawned process.
Values are `${var}`-expanded.

### `workspace`

| Key    | Type    | Default | Meaning |
|--------|---------|---------|---------|
| `cwd`  | string  | `"."`   | Working directory, relative to the scenario file's directory. Ignored when `temp` is true. |
| `temp` | boolean | `false` | Run inside a fresh `0700` temp directory, removed when the run ends. |

### `matrix`

A map from axis name to a non-empty list of string values. Each axis name must
appear somewhere reachable by `${axis}` expansion (a spawn command, a `send`,
or an `env`/`spawn.env` value). `ptytest matrix` runs the scenario once per
element of the Cartesian product of all axes, injecting each cell's values as
the same-named variables. See the README's *Matrix* section for the full rules.

> **(S-2) Axis values are strings.** Each value is deserialized as a string and
> injected as the literal text of `${axis}`. A YAML numeric value such as
> `[1, 2]` is therefore **stringified** to `"1"`/`"2"` before injection — there
> is no numeric matrix axis. Quote values that must keep a specific textual form
> (e.g. `["01", "1.0"]`).

## `${var}` expansion

`${name}` placeholders are expanded in:

- `spawn.command` and `spawn.env` values,
- `send` and `send_raw` payloads,
- scenario-level `env` values.

Resolution order for `${name}`:

1. a scenario `variables` entry named `name` (plain or secret value);
2. otherwise the parent process environment variable `name`;
3. otherwise the placeholder is left **literal** (`${name}`).

Use `$$` for a literal `$`.

## Steps

Each step is a one-key map whose key names the step kind. Exactly one key per
step; an empty step or a step with two keys is a Scenario error, and an unknown
step name is a Scenario error.

| Step | Value shape | Meaning |
|------|-------------|---------|
| `spawn` | string, or `{command, cwd?, env?}` | Start a child process in the PTY. `command`'s first token is the program. `${var}` expanded. |
| `send` | string | Write a line to stdin; a trailing `\r` (Enter) is appended. `${var}` expanded. |
| `send_raw` | string | Write bytes to stdin with **no** trailing terminator. `${var}` expanded. |
| `key` | string | Send a named key as its control byte(s). See the key set below. |
| `wait` | duration string | Sleep a fixed duration (`2s`, `500ms`). |
| `expect` | `{contains, timeout?}` | Wait until output contains `contains`, up to `timeout`. |
| `expect_regex` | `{pattern, timeout?}` | Wait until output matches the regex `pattern` (matched against output bytes). |
| `expect_not` | `{contains}` | Assert immediately that pending output does **not** contain `contains`. Takes **no `timeout`** (see below). |
| `expect_file_exists` | `{path}` | Assert a file exists (path relative to workspace cwd). |
| `expect_file_contains` | `{path, contains}` | Assert a file's contents contain a substring. |
| `expect_file_not_contains` | `{path, contains}` | Assert a file's contents do **not** contain a substring. |
| `expect_file_changed` | `{path}` | Assert a file's contents differ from spawn time. |
| `expect_exit` | integer, or `{code, timeout?}` | Assert the child exited with `code`. The struct form polls up to `timeout`; the bare integer polls once. |
| `expect_running` | boolean | Assert whether the child is still running. |
| `expect_json` | `{path, <one check>, source?, timeout?}` | Assert on a JSON value at `path`. See below. |
| `expect_snapshot` | `{file, raw?}` | Compare current output to a recorded snapshot file. `raw: true` compares bytes; otherwise ANSI is stripped first. |
| `expect_semantic` | `{text, similarity, source?}` | Assert output is at least `similarity` (0.0–1.0) close to `text`. |

### `expect_not` takes no `timeout`

Unlike `expect`/`expect_regex`, which wait up to a `timeout` for output to
appear, `expect_not` is an **immediate** check: it asserts that the output
captured *so far* does not contain `contains`, and never waits. It therefore has
**no `timeout` field**. The JSON schema sets `additionalProperties: false` on
`expect_not`, so an editor flags a stray `timeout`. At run time a stray field is
**ignored** (not an error), consistent with the forward-compatibility policy
that keeps step specs lenient (see [`COMPATIBILITY.md`](COMPATIBILITY.md)). To
wait for a substring to be present, use `expect`; to assert absence after a
delay, precede `expect_not` with a `wait`.

### `expect_json` checks

`expect_json` requires a `path` (dotted, e.g. `result.items.0.name`) and
**exactly one** of:

- `equals: <value>` — typed equality (`200` is a number, `"200"` a string);
- `contains: <string>` — substring of the value's string form;
- `exists: true` — the path resolves to a value.

Specifying zero or more than one check is a Scenario error. `source` selects
where the JSON is read from:

- `source: output` (the default) — the trailing JSON block of live output;
- `source: {file: <path>}` — a workspace-relative file.

`timeout` applies only to `source: output` (how long to wait for a tail JSON
block to appear). An unrecognized `source` keyword is a Scenario error.

### `source` (shared by `expect_json` and `expect_semantic`)

`source` is either the bare string `output` (default) or a mapping
`{file: <path>}`. Any other bare string is a Scenario error.

### (S-3) Unknown fields inside a step/spec are ignored, not rejected

`deny_unknown_fields` is enforced **only at the top level** (so `stesp:` fails
loudly). Inside a step or spec, an unrecognized field is **silently ignored**,
not an error — this is what keeps a scenario authored for a newer `1.x`
(carrying an additive optional field) parsing on an older ptytest (see
[`COMPATIBILITY.md`](COMPATIBILITY.md)). The trade-off is that a *typo* inside a
spec (e.g. `contians:` for `contains:`) is not caught by ptytest at run time;
rely on the JSON schema (and your editor's YAML language server) to flag it. The
schema sets `additionalProperties: false` where a spec's field set is closed
(e.g. `expect_not`), turning such typos into editor warnings.

### Key names

`key:` accepts these names, case-insensitively (surrounding whitespace ignored):

| Name | Bytes |
|------|-------|
| `enter` | `\r` (`0x0D`) |
| `tab` | `\t` (`0x09`) |
| `escape` / `esc` | `0x1B` |
| `backspace` | `0x7F` |
| `up` | `ESC [ A` |
| `down` | `ESC [ B` |
| `right` | `ESC [ C` |
| `left` | `ESC [ D` |
| `ctrl+c` | `0x03` |
| `ctrl+d` | `0x04` |
| `ctrl+z` | `0x1A` |

An unrecognized key name is a Scenario error.

## Compatibility (SemVer)

ptytest version `1.0.0` freezes two separate contracts. They are versioned
together by the crate version but evolve under distinct rules. See
[`COMPATIBILITY.md`](COMPATIBILITY.md) for the full statement.

- **Input (this scenario format)** is the stability target. Within `1.x`,
  ptytest only **adds** optional steps and optional fields. Removing a step or
  field, changing the meaning of one, or making an optional field required is a
  breaking change reserved for `2.0`. A scenario valid under `1.0.0` stays valid
  under every `1.x`.
- **Output (the report JSON)** is a separate contract: `Report`,
  `MatrixReport`, and `BenchReport`. Adding a field is a minor change; removing
  a field, changing its type, or changing its meaning is major. **Consumers must
  ignore unknown fields** (Robustness Principle) so an additive change does not
  break them.
