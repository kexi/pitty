# pitty scenario format (stable, v1)

This document specifies the **stable scenario input format** as of pitty
`1.0.0`. A scenario is a single YAML document. Unless noted otherwise, every
key shown is optional with the stated default.

A machine-readable JSON Schema for editor autocompletion and validation lives
at [`schema/pitty-scenario-v1.json`](schema/pitty-scenario-v1.json). Point
your YAML language server at it with a modeline at the top of a scenario file:

```yaml
# yaml-language-server: $schema=./schema/pitty-scenario-v1.json
```

(Use a relative path that resolves to the schema in this repo, or a checked-in
copy. pitty does not publish the schema to an external URL.)

The schema aims to reach the **same** accept/reject verdict as pitty itself: a
scenario your editor shows as valid should run, and one pitty rejects with a
Scenario error should be flagged before you run it. `tests/schema_contract.rs`
enforces that by validating a table of scenarios against the shipped schema
with a real draft-07 validator and comparing each verdict to pitty's own. The
exception is anything only a live run can know (a missing file, a command that
never exits) — the schema judges shape, not the world.

## Top-level keys

A scenario document accepts exactly these seven keys. **Any other top-level key
is a Scenario error** (exit code 2): pitty applies `deny_unknown_fields` at
the top level so a typo such as `stesp:` (for `steps:`) fails loudly instead of
leaving an empty step list that passes vacuously.

| Key         | Type                          | Default | Meaning |
|-------------|-------------------------------|---------|---------|
| `version`   | integer                       | `1`     | Scenario format version (see below). |
| `name`      | string (**required**)         | —       | Human-readable name, shown by `list` and in reports/logs. |
| `variables` | map of name → var spec        | `{}`    | Values for `${var}` expansion; a variable may be marked secret. |
| `env`       | map of string → string        | `{}`    | Environment variables injected into every spawned process. |
| `workspace` | workspace spec                | `{cwd: ".", temp: false}` | Where commands run. |
| `matrix`    | map of axis → list of strings | `{}`    | Matrix axes; cells are the Cartesian product (run with `pitty matrix`). |
| `steps`     | list of steps                 | `[]`    | Ordered steps to execute. |

### `version`

`version` is an optional integer. When omitted it defaults to `1`. This build
supports **version 1 only**. A scenario declaring any other version (e.g.
`version: 2`) is rejected with a Scenario error:

```
unsupported scenario version 2; this pitty supports version 1 (update pitty for newer scenarios)
```

This is deliberate: silently parsing a newer scenario could drop steps or field
semantics the new version introduced and report a false pass. Update pitty to
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
Values are `${var}`-expanded, by the same rules as every other expansion site
(see [`${var}` expansion](#var-expansion)).

> **(E-1) `env` values cannot reference each other.** Expansion resolves a
> `${name}` against scenario `variables` and the parent process environment
> only, never against a sibling `env` key. `env: {A: "base", B: "${A}/sub"}`
> leaves `B` as the literal `${A}/sub` unless `A` is also a `variables` entry or
> a parent-env variable. Declare the shared part as a `variables` entry and
> reference it from both.

### `workspace`

| Key    | Type    | Default | Meaning |
|--------|---------|---------|---------|
| `cwd`  | string  | `"."`   | Working directory, relative to the scenario file's directory. Ignored when `temp` is true. |
| `temp` | boolean | `false` | Run inside a fresh temp directory, removed when the run ends (`0700` on Unix). |

### `matrix`

A map from axis name to a non-empty list of string values. Each axis name must
appear somewhere reachable by `${axis}` expansion (a spawn command, a `send`,
or an `env`/`spawn.env` value). `pitty matrix` runs the scenario once per
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
step name is a Scenario error. The JSON schema sets `additionalProperties:
false` on the step map, so a misspelled step name (`expct:` for `expect:`) is
flagged in the editor rather than only at run time.

| Step | Value shape | Meaning |
|------|-------------|---------|
| `spawn` | string, or `{command, cwd?, env?, split?}` | Start a child process in the PTY. `command` is split on whitespace by default; `split: posix` opts into POSIX shell word rules. The first word is the program. `${var}` expanded. See [`spawn` command tokenization](#spawn-command-tokenization). |
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

### `spawn` command tokenization

`command` is a **single string** split into `[program, args...]`. Which rule is
used is chosen per `spawn` by the optional **`split`** field:

| `split` | Rule |
|---------|------|
| `whitespace` (**default**) | Split on any run of whitespace. Quotes and backslashes are ordinary bytes. |
| `posix` | POSIX shell word rules. Quotes and backslash escapes group and are consumed. |

`split` is only available in the **mapping form** of `spawn`. The bare string
form (`spawn: echo hi`) always uses the default, because the string is the
command line itself and has nowhere to carry a modifier. The keyword is matched
case-insensitively after trimming, like `key` and `source`.

Unlike `key` and `source`, an **unrecognized `split` value is not an error**: it
falls back to the default rule and prints a warning on stderr. This holds for
any value of **any type** — `split: 42`, `split: [a, b]` and `split: {x: y}` all
warn and use the default rather than failing the scenario. This is required by
the compatibility contract rather than chosen — `split` postdates `1.0`, so
every earlier `1.x` accepts any value at that key, of any type, and ignores the
field; this build may not be stricter than the builds already in the field. See
[`COMPATIBILITY.md`](COMPATIBILITY.md) for the general rule.

The corollary is that **`split: posix` is silently ignored by a pitty that
predates the field**, which tokenizes the command with the default rule instead.
Such a scenario still runs on the older build but does not mean the same thing
there; in practice it fails, because the assertions that motivate the opt-in are
the ones that break without it. Pin a minimum pitty version in CI if you need a
hard guarantee.

#### Why `whitespace` is the default

POSIX rules are the ones most authors expect, and `whitespace` is a footgun:
`spawn: "echo 'hello world'"` makes the child print the quote characters, and
`spawn: "sh -c 'exit 3'"` runs the program `'exit`. But
[`COMPATIBILITY.md`](COMPATIBILITY.md) promises that within `1.x` pitty will not
change the meaning of an existing field or turn a previously valid scenario into
an error. Scenarios written against `1.0` have recorded snapshots of the quoted
output and assertions on the resulting exit codes, so flipping the default would
break them on a patch upgrade. The correct rule is therefore offered additively;
a future `2.0` may make it the default.

#### `split: posix`

```yaml
steps:
  - spawn:
      command: "sh -c 'exit 3'"
      split: posix
  - expect_exit: 3
```

Under `posix`:

- `'single quotes'` group verbatim (no escapes inside);
- `"double quotes"` group and recognize backslash escapes;
- a backslash escapes the next character outside quotes;
- any run of unquoted whitespace separates words.

The quote and escape characters are **grouping syntax**: they are consumed, not
passed to the child. So `command: "echo 'hello world'"` with `split: posix` runs
`echo` with the single argument `hello world`, and `command: "sh -c 'exit 3'"`
hands `sh` the script `exit 3` as one argument.

A command line that cannot be tokenized under `posix` — most commonly an
unterminated quote (`command: "echo 'oops"`) — is a **Process error (exit code
3)**, not a silent best-effort split. Under the default `whitespace` rule the
same line has no invalid form: it simply splits into mangled words.

A command containing no quotes or backslashes tokenizes identically under both
rules, so adding `split: posix` to such a `spawn` is a no-op.

#### No shell is interposed (either way)

pitty tokenizes in-process and execs the program directly, so the program under
test is the direct PTY child. Shell features — globbing, redirection (`>`),
pipes (`|`), `&&`, and shell variable expansion — are therefore **not**
available in `command` itself; run them through an explicit shell
(`command: "sh -c 'a > b'"`, `split: posix`), where the shell you name does the
interpreting. Note that `${var}` is expanded by pitty (see
[`${var}` expansion](#var-expansion)) **before** tokenization, so an expanded
value containing whitespace or quote characters participates in the split.

#### `split: posix` applies on every platform, Windows included

`cmd.exe` quoting conventions are *not* used there. The reason is that pitty
builds an argv vector, not a command string: the argv is handed to portable-pty,
which performs the Windows argv-to-command-line re-quoting itself. Applying host
shell rules per platform would make one scenario file mean two different things
on two runners and break the cross-platform promise. The practical consequence
on Windows is that backslashes in a path are escape characters like anywhere
else, so quote such a path:

```yaml
steps:
  # POSIX single quotes around the path: nothing inside them is interpreted,
  # so the backslashes reach the child verbatim.
  - spawn:
      command: "'C:\\Program Files\\app.exe' --flag"
      split: posix
```

Mind the two layers here. YAML resolves its own escapes first (inside a
double-quoted YAML scalar `\\` is one backslash), and the resulting string is
then tokenized. The form above yields the argv
`["C:\Program Files\app.exe", "--flag"]`. Writing the path **unquoted** does not
work under `posix` — `command: 'C:\Program Files\app.exe --flag'` is a
single-quoted *YAML* scalar, so the tokenizer sees bare backslashes, consumes
them as escapes, and produces `["C:Program", "Filesapp.exe", "--flag"]`. Under
the default `whitespace` rule the backslashes survive but the path is still torn
at the space, so a Windows path with a space needs `split: posix`.

There is currently **no array form** of `spawn.command` (`{argv: [...]}`), so
`split: posix` plus quoting is the only way to pass an argument containing
whitespace. Prefer single quotes around any such argument: they are the
unambiguous form under these rules, since nothing inside them is interpreted.

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
  `<value>` may be a nested YAML map or list, which deserializes to the
  equivalent typed JSON structure and compares the whole value at `path`;
- `contains: <string>` — substring of the value's string form;
- `exists: true` — the path resolves to a value.

Specifying zero or more than one check is a Scenario error, and so is
`exists: false` (there is no negated form; use `expect_json` on a sibling path
or an `expect_not`). The JSON schema encodes this one-of as a `oneOf` over the
three check fields, so an editor flags a zero-check or multi-check spec before
the run. `source` selects where the JSON is read from:

- `source: output` (the default) — the trailing JSON block of live output;
- `source: {file: <path>}` — a workspace-relative file.

`timeout` applies only to `source: output` (how long to wait for a tail JSON
block to appear). An unrecognized `source` keyword is a Scenario error.

#### Where the JSON block must sit (`source: output`)

With `source: output`, pitty searches the **last 64 KiB** of output and returns
the last block that parses. Three bounds are worth knowing:

- **Block size is not capped** below that window — a report of any size up to
  the 64 KiB window extracts, provided it *ends* near the tail.
- **Staleness is capped**: a block whose closing `}`/`]` sits more than **8 KiB**
  before the end of output is never returned. This keeps a truncated final
  report from silently resolving to an older, unrelated block further up the log.
- **Nesting depth is capped** at the JSON parser's own recursion limit (127
  levels). A document nested deeper is reported as "no valid JSON block" — it is
  never partially extracted, and in particular the deepest inner portion that
  *would* parse on its own is not returned in place of the real document.

Extraction either returns the complete outermost block or reports that it found
none; it never returns an inner fragment of the block it was looking at. That
holds for every reason a block can fail — malformed, too deep, or too costly to
analyse.

Note the one deliberate exception, which is about *different* blocks rather than
fragments of one: if the final block is **malformed**, an earlier complete block
within the 8 KiB staleness window may be returned instead (see above). A block
that could not be *analysed* never triggers that fallback — if pitty gives up on
the region nearest the tail, it reports no result rather than answering with an
older block that might not be the one you meant.

Deep or malformed output elsewhere in the log does not interfere with a good
report: only material that overlaps the report, or that sits between it and the
end of output, can affect whether it is returned. Several such stretches are
judged independently, so a report is not refused merely for sitting between two
unrelated ones.

Quoting in the surrounding log text cannot hide a real report. A block is found
by its own structure, so unbalanced, escaped, or stray `"` characters in log
lines cannot make a report disappear or cause an older one to be returned in its
place.

A JSON-looking run inside quoted log prose — say `log: "{"status":"ok"}"` — is
ranked below any block that is **not** inside quoted prose, so it cannot displace
a genuine report appearing anywhere in the output, even when the quoted run sits
closer to the tail. It is extracted only when the output contains no real JSON
block at all. (Quoting is judged per line, so a stray quote on one log line does
not change how the next line is read.)

Quoting is judged with the same escape rule JSON uses: inside a quoted run a
backslash escapes the next character, so an escaped quote does **not** end the
run. A logged message that quotes something itself — `log: "he said \" then
stopped"` — therefore reads as one quoted message rather than flipping at the
escape, and a real report elsewhere in the output keeps its rank. Outside a
quoted run a backslash is ordinary text and escapes nothing.

An **unquoted** later block still wins on tail position, as always: if a run
prints two real reports, the last one is used.

So the rule for authors is about *placement*, not length: print the report at the
very tail of output, with nothing after it but a newline and at most a few lines
of trailing log. A large report is fine; a report followed by more than 8 KiB of
subsequent output is not.

### `source` (shared by `expect_json` and `expect_semantic`)

`source` is either the bare string `output` (default) or a mapping
`{file: <path>}`. Any other bare string is a Scenario error.

The keyword is matched **case-insensitively after trimming** surrounding
whitespace, exactly like `key:` — `Output`, `OUTPUT` and `  output  ` all mean
`output`. An empty string (`source: ""`) also means the default `output`, so a
present-but-blank value is lenient rather than fatal. The JSON schema encodes
the same normalization as a pattern, so schema-based tooling and pitty agree on
every accepted spelling.

### (S-3) Unknown fields inside a step/spec are ignored, not rejected

`deny_unknown_fields` is enforced **only at the top level** (so `stesp:` fails
loudly). Inside a step or spec, an unrecognized field is **silently ignored**,
not an error — this is what keeps a scenario authored for a newer `1.x`
(carrying an additive optional field) parsing on an older pitty (see
[`COMPATIBILITY.md`](COMPATIBILITY.md)). The trade-off is that a *typo* inside a
spec (e.g. `contians:` for `contains:`) is not caught by pitty at run time;
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

pitty version `1.0.0` freezes two separate contracts. They are versioned
together by the crate version but evolve under distinct rules. See
[`COMPATIBILITY.md`](COMPATIBILITY.md) for the full statement.

- **Input (this scenario format)** is the stability target. Within `1.x`,
  pitty only **adds** optional steps and optional fields. Removing a step or
  field, changing the meaning of one, or making an optional field required is a
  breaking change reserved for `2.0`. A scenario valid under `1.0.0` stays valid
  under every `1.x`.
- **Output (the report JSON)** is a separate contract: `Report`,
  `MatrixReport`, and `BenchReport`. Adding a field is a minor change; removing
  a field, changing its type, or changing its meaning is major. **Consumers must
  ignore unknown fields** (Robustness Principle) so an additive change does not
  break them.
