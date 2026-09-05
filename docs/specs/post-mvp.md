# Post-MVP input and reporting contracts

Status: entry contract for future feature issues. The capabilities in this
document are not implemented by the MVP.

Issue #53 tracks the capabilities that cross the MVP input, runtime, output,
or write boundary. A feature issue must select one capability, implement its
compatibility and privacy tests, and pass the relevant safety checks before it
changes the current command surface.

## Shared boundary

Every capability keeps the existing flow:

```text
upstream input -> adapter -> canonical records -> derived store -> lens/report
```

- Upstream field names stop at the adapter.
- Raw rollout/state inputs and application source files are source read-only:
  refreshes and reports may read them but never rewrite, rename, truncate,
  repair, or delete them. The only future write exception is the explicit,
  validated instruction-target contract in section 5.
- Processing stays local and deterministic. No network or LLM is required.
- Missing, malformed, unsupported, and incomplete input is explicit and
  bounded; it must not silently discard unrelated sources.
- Human-readable output remains the default. Machine-readable output is an
  explicit opt-in with a versioned schema.
- Tests use bounded synthetic data only. Real prompts, commands, outputs,
  tokens, and personal identifiers must not enter fixtures or reports. Source
  paths and line numbers may appear where the existing evidence contract
  permits them; raw source content may not.

## Current MVP regression coverage

The executable guards for the still-deferred boundary are intentionally
negative or read-only until a capability issue is explicitly agreed:

- `compressed_rollout_input_is_explicitly_unsupported_and_read_only` checks
  the current unsupported-reader diagnostic, bounded privacy behavior, and
  unchanged compressed source.
- `refresh_and_frozen_reporting_are_not_currently_exposed`,
  `machine_readable_output_is_not_currently_exposed`,
  `live_monitoring_is_not_currently_exposed`, and
  `optimize_apply_is_not_currently_exposed` check each deferred CLI boundary
  independently; rejected errors stay bounded and stores stay unchanged.
- `reporting_is_deterministic_bounded_and_does_not_refresh_or_write` checks
  repeated human-readable output, bounded/redacted evidence, and unchanged
  derived/raw files.
- `reporting_commands_render_local_store_data` and
  `optimize_diff_renders_a_proposal_without_writing_the_target` cover the
  current deterministic human-readable, bounded-evidence, and read-only
  reporting behavior.

The positive compatibility/privacy cases listed in each section are required
executable tests in the feature issue that introduces that capability. This
contract-only change does not pretend that an unimplemented capability has
runtime behavior to test.

## 1. Compressed rollout readers

### Scope

Add a compressed rollout reader in the adapter only. A `.jsonl.zst` source
must yield the same logical `(line number, bytes)` stream as its equivalent
plain `.jsonl` source, then reuse the existing JSONL parser, normalizer, and
store transaction. Compression details must not appear in canonical records,
lenses, or reports.

The source identity is computed from the compressed source bytes. A changed
compressed source is replaced as one derived-source transaction; an unchanged
source is skipped. Decompression must retain the existing maximum-line bound
after decompression and must not allocate an unbounded line.

### Compatibility tests

- Equivalent plain and compressed synthetic rollouts produce equal canonical
  facts and equivalent diagnostics, apart from source path and reader
  provenance.
- Unknown records, missing optional fields, malformed JSON, and oversized
  lines have the same continue-with-diagnostic behavior for both readers.
- Corrupt or truncated compressed data produces one bounded diagnostic for
  that source and does not prevent a valid sibling source from being ingested.
- Re-ingesting unchanged compressed bytes is skipped; changing the bytes
  replaces only that source's derived rows.

### Privacy tests

- Decompressed prompt, command, and tool-output content never appears in a
  reader error or a human-readable report without the existing bounded and
  redacted excerpt path.
- Synthetic compressed fixtures contain no real session data, credentials, or
  private paths.
- The reader never writes to the compressed source while reading or reporting
  a diagnostic.

## 2. Refresh and frozen reporting

### Scope

Introduce an explicit refresh workflow for building or updating the derived
store from discovered inputs. Refresh reads raw rollout/state and instruction
sources, applies the existing identity and incremental-ingest rules, and
commits all replacements atomically. A failed source read or transaction
leaves the previous successful derived state available.

Reporting remains a separate operation over the derived store. A future
`--frozen` reporting mode means "use exactly this store": it must not discover
or reopen raw inputs, refresh the store, or silently claim that the store is
current. Missing or invalid stores remain bounded errors; recorded freshness
is shown in the report.

The refresh command and `--frozen` option must be explicit in the feature
issue that implements them. This document defines their boundary, not an
additional implicit refresh on an existing reporting command.

### Compatibility tests

- Refreshing an unchanged synthetic source does not duplicate sessions,
  records, snapshots, or diagnostics.
- Refreshing a changed source replaces only that source's derived rows and
  preserves unrelated sources.
- A failed or interrupted replacement leaves the previous derived state and
  freshness record intact.
- `--frozen` produces the same report for the same store regardless of raw
  source changes and never reads or modifies those raw sources.
- Reporting without `--frozen` does not gain an implicit refresh as a side
  effect; the explicit refresh path is the only writer of derived state.

### Privacy tests

- Refresh diagnostics are bounded and redact prompt, command, and output
  excerpts using the existing canonical/report limits.
- Frozen and normal reports contain only derived-store data and permitted
  bounded evidence; they do not transmit data or read outside the configured
  input boundary.
- Both workflows preserve the source read-only guarantee, including on parse,
  migration, and rollback errors.

## 3. Machine-readable output

### Scope

Add an explicit `--format json` opt-in to the supported reporting commands.
The default human-readable format remains unchanged. JSON output is one
document on stdout; diagnostics and operational errors stay on stderr and are
not mixed into the document.

The top-level JSON contract is versioned and uses stable snake-case fields:

```json
{
  "schema_version": 1,
  "command": "doctor",
  "data": {}
}
```

`data` has one of these command-specific shapes:

- Finding commands (`analyze`, `failures`, `corrections`, `rework`, `stuck`,
  `verification`, `knowledge`, `rediscovery`, `instructions`, and `doctor`)
  use `{period_start, period_end, session_count, freshness, finding_counts,
  groups}`. `freshness` is `{state, source_count, latest_ingested_at}`;
  `groups` is an ordered array of `{scope, findings}`; each finding contains
  the typed `Finding` fields `kind`, `severity`, `confidence`, `scope`, `key`,
  `summary`, `evidence`, `occurrences`, `distinct_sessions`,
  `affected_paths`, `observed_commands`, `sequence`, `suggested_action`,
  `limitations`, and `verification_status`, plus `heuristic`.
- `sessions` uses `{freshness, sessions}`, where each session is
  `{id, created_at, updated_at, cwd, project}`.
- `optimize --diff` uses `{rendered, skipped}`, where `rendered` contains the
  typed proposal and unified `diff`, and `skipped` contains
  `{target_path, reason}`.

All wrapper fields above are required. `string`, `integer`, and `boolean` use
their JSON primitive types; nullable values are `string | null` or
`object | null` as stated. `freshness.state` is `"empty" | "recorded"`, and
`latest_ingested_at` is `string | null`. Finding `kind` values are
`failure | correction | rework | stuck | verification | knowledge | gap |
overscoped | duplicate | stale | truncated`; `severity` and `confidence` are
`low | medium | high`; `verification_status` is `missing | not_observed | null`.
The `command` value is canonical: `stuck` serializes as `rework` and
`rediscovery` as `knowledge`, matching their aliases.

The complete required top-level types are `schema_version: integer`,
`command: analyze | sessions | failures | corrections | rework | verification |
knowledge | instructions | doctor | optimize_diff`, and `data: object`.
Finding-report data has `period_start: string | null`,
`period_end: string | null`, `session_count: non-negative integer`,
`freshness: Freshness`, `finding_counts: object<string, non-negative integer>`,
and `groups: FindingGroup[]`. `Freshness` has required
`state: empty | recorded`, `source_count: non-negative integer`, and
`latest_ingested_at: string | null`.

`FindingGroup` is `{scope: Scope, findings: Finding[]}`. `Finding` has the
following required fields and types: `kind: string enum`, `severity: string
enum`, `confidence: string enum`, `scope: Scope`, `key: string`,
`summary: string`, `evidence: Evidence[]`, `occurrences: non-negative integer`,
`distinct_sessions: non-negative integer`, `affected_paths: string[]`,
`observed_commands: string[]`, `sequence: string[]`,
`suggested_action: string`, `limitations: string[]`, and
`verification_status: missing | not_observed | null`.

`scope` is an object with `{kind: "global"}` or
`{kind: "project" | "instruction" | "path", value: string}`. An evidence
item is `{session_id: string | null, source, role, excerpt: string | null}`;
`source` is `{kind: "rollout" | "state", path: string, line: integer | null,
ingested_at: string | null, parser_schema_version: integer}`. Evidence roles
are `observation | preceding_action | file_operation | verification_command |
instruction_snapshot | instruction_file`. All fields in these objects are
required, including nullable fields.

Each `groups` element is `{scope, findings}` with both fields required. Each
finding element has all of the listed `Finding` fields required; only
`verification_status` is nullable. Its `evidence`, `affected_paths`,
`observed_commands`, `sequence`, and `limitations` fields are arrays of the
types named by their fields, and each array is present even when empty. Each
`sessions` element has required `id: string`, `created_at: string | null`,
`updated_at: string | null`, `cwd: string | null`, and `project: string | null`.
`heuristic` and `diff` are required strings. The wrapper's `rendered` and
`skipped` arrays are present even when empty.

The `sessions` data object is `{freshness: Freshness, sessions: Session[]}`;
`Session` is `{id: string, created_at: string | null, updated_at: string | null,
cwd: string | null, project: string | null}`. A `RenderedDiff` is
`{proposal: Proposal, diff: string}`. A `Proposal` has required
`target_scope: Scope`, `target_path: string`, `action: string enum`,
`observed_problem: string`, `evidence_count: non-negative integer`,
`distinct_sessions: non-negative integer`, `confidence: string enum`,
`heuristic: string`, `evidence: Evidence[]`,
`proposed_text: string | null`, `existing_text: string | null`,
`source_path: string | null`, `expected_target_hash: string | null`,
`expected_source_hash: string | null`, `target_rationale: string`,
`limitations: string[]`, and `review_reminder: string`. A `SkippedProposal` is
`{target_path: string, reason: string}`. `rendered` and `skipped` are arrays
of those exact element types.

For `optimize --diff`, `action` is `add | modify | remove | move_to_docs |
split_scope`; all fields are required, including nullable fields, as defined
by `Proposal` above.

`groups`, `findings`, `evidence`, `affected_paths`, `observed_commands`,
`sequence`, `limitations`, `sessions`, `rendered`, and `skipped` are arrays;
`finding_counts` is an object from finding-kind strings to non-negative
integers. `finding_counts` may omit zero-valued kinds. Arrays are ordered as
documented below and never omitted when empty.

The human-readable baseline is line-oriented UTF-8 with LF line endings, no
ANSI control sequences, and exactly one final LF. Its grammar is:

```text
Analyzed period: <unknown | timestamp | timestamp .. timestamp>\n
Sessions: <non-negative integer>\n
Store freshness: <empty | recorded | recorded at timestamp> (<integer> source files)\n
Finding counts: <none | kind=integer[, kind=integer...]>\n
\n[<scope>]\n
- <kind> / <severity> / <confidence>: <bounded summary> (<integer> occurrences, <integer> sessions)\n
  heuristic: <bounded text>\n
  action: <bounded text>\n
  evidence: <path[:line]>[ — <bounded excerpt>]\n
  limitation: <bounded text>\n```

The blank line and repeated evidence/limitation lines are omitted when their
containing group or list is empty. Groups use global, project, instruction,
then path order; findings use the existing deterministic order (severity,
confidence, distinct sessions, normalized key, occurrences, kind/key, then
scope). `sessions` uses `Store freshness`, `Sessions`, then one `- id` block
with `created`, `updated`, `cwd`, and `project` lines. `optimize --diff` writes
each `Proposal ...` summary and unified diff to stdout and each `Skipped ...`
line to stderr. Bounded text is redacted before rendering; future multiline
values must be escaped rather than creating extra grammar lines. The existing
[`analysis.md`](analysis.md) and README command table remain authoritative for
the command list and current examples.

The command-specific empty and alias forms are exact:

- Finding commands (`analyze`, each focused lens, `doctor`, and their aliases)
  emit only the four header lines above when there are no groups; the fourth
  line is exactly `Finding counts: none`.
- `sessions` emits exactly `Store freshness`, `Sessions: <n>`, and, for each
  session in lexicographic `id` order, `- <id>` followed by exactly
  `created`, `updated`, `cwd`, and `project` lines. It emits no session block
  when `<n>` is zero.
- `optimize --diff` emits `No applicable proposals.` followed by one LF when
  both rendered and skipped sets are empty. Otherwise it emits each rendered
  proposal in target/action order, with this exact prefix before its standard
  unified diff:

  ```text
  Proposal <action> <target>\n
  Observed: <bounded text>\n
  Evidence: <integer> occurrences across <integer> sessions\n
  Confidence: <low | medium | high>\n
  Heuristic: <bounded text>\n
  Target: <bounded text>\n
  Limitation: <bounded text>\n
  Evidence ref: <path[:line]>[ — <bounded excerpt>]\n
  <review reminder>\n
  <unified diff>\n
  ```

  `Limitation` and `Evidence ref` repeat in source order when present. Each
  skipped proposal is one `Skipped <path>: <reason>` line on stderr, sorted by
  path then reason. Alias commands use the canonical output form. Text and
  path values escape backslash, tab, carriage return, and line feed as
  `\\`, `\t`, `\r`, and `\n` respectively after redaction and truncation.

Unknown optional fields are ignored by readers, missing optional values are
`null`, arrays use the same deterministic ordering as the human report, and
map keys are sorted.
Schema changes require a new version and a compatibility note; output order
must not depend on SQLite row order or hash-map iteration.

Human and machine-readable output share the same bounded, redacted evidence
path. Raw unknown records and unbounded prompt, command, or tool-output text
are never emitted by default.

### Compatibility tests

- Repeating a JSON report with the same synthetic store and options produces
  byte-for-byte identical output.
- The same data has the same finding counts, scopes, evidence references, and
  ordering in human-readable and machine-readable modes.
- The default human-readable output remains compatible when JSON support is
  added; selecting JSON is the only behavior change.
- A declared schema-version change is the only way to change required field
  meaning, and a decoder ignores unknown optional fields.

### Privacy tests

- Synthetic secret-like values and long excerpts are absent or redacted in
  both formats, with the documented byte limits enforced.
- JSON diagnostics and errors do not include raw session payloads, tokens, or
  credentials.
- Local source paths appear only where the existing evidence contract permits
  them; no network destination or remote-upload field is introduced.

## 4. Live monitoring

### Scope

Add live monitoring only as an explicit local runtime boundary. It may observe
append-only rollout/state changes and produce incremental findings, but it must
reuse the adapter and canonical model; it must not create a second parser or
leak upstream event names into lenses.

The contract must define the input lifecycle before implementation: source
identity and offsets, incomplete final lines, rotation/truncation, duplicate
events, stop behavior, restart behavior, and the distinction between partial
observations and a completed session. A monitor may not silently turn a
partial observation into a final finding.

Monitoring is read-only with respect to sources, local-only, and bounded. It
must not require a hosted service, background daemon, or unbounded in-memory
history for the first implementation.

### Compatibility tests

- Replaying a finite synthetic event stream through the monitor produces the
  same canonical ordering and findings as batch ingestion.
- A partial final line is held until completion; rotation, truncation, and
  duplicate source identity are explicit diagnostics or documented state
  transitions, never silent data loss.
- Stop and restart from a recorded offset do not duplicate or skip complete
  synthetic events.
- The monitor has a deterministic test clock/stop boundary and does not make
  tests depend on wall-clock timing.

### Privacy tests

- Live output uses the same bounded and redacted evidence rules as reports.
- The monitor never sends data over the network and never writes, repairs, or
  deletes the observed source.
- A stopped monitor releases its source handles and does not retain raw
  session payloads beyond the documented derived-store boundary.

## 5. `optimize --apply`

### Scope

Keep `optimize --diff` review-only and read-only. A future `optimize --apply`
may write only validated proposal targets in the allowed instruction scope;
it must never write rollout files, state databases, source files, or the
derived store as a side effect of applying a proposal.

Before any write, the implementation must:

1. require an explicit confirmation step; interactive use confirms the exact
   target set, while non-interactive use must provide `--yes` after the diff
   was reviewed;
2. re-read every target and verify its expected content hash;
3. validate the generated patch against that exact target, with no fuzzy or
   partial application;
4. validate target scope, regular-file status, and symlink/path boundaries;
5. create recoverable backups before the first target write.

Writes must be atomic per target. If any target write fails, the workflow must
restore already changed targets from the backups, report recovery status, and
return failure. Backups remain available after a successful run; the initial
implementation must not delete them implicitly. A separate, explicit cleanup
policy may be specified later. A successful result must identify the targets
changed and the backup/recovery outcome.

### Compatibility tests

- Missing confirmation, a changed target hash, an invalid patch, or a scope
  violation performs no target write and no partial apply.
- A synthetic multi-target failure restores every target changed before the
  failure and leaves backups available for inspection.
- A successful apply changes only the expected target bytes; `--diff` remains
  byte-for-byte read-only and continues to render the same proposal.
- Rollout/state files and the derived store are byte-for-byte unchanged by
  both successful and failed apply attempts.

### Privacy tests

- Confirmation, success, failure, backup, and recovery messages contain
  bounded paths and summaries, never raw session prompts, commands, outputs,
  tokens, or credentials.
- Backups are local, scoped to the validated target, and are never uploaded or
  copied into repository fixtures.
- Recovery failures are explicit and actionable; the command never reports
  success while a target or backup is in an unknown state.

## Entry gate for implementation issues

Before a feature issue implements one section, it must name that section and
record that the relevant acceptance criteria and boundary are explicitly
agreed in the issue or PR before implementation starts, then add the listed
compatibility and privacy tests with synthetic data. It must
also update the relevant adapter, canonical, store, lens, or report
specification, preserve the source read-only boundary, run the pinned CI
commands, and record any newly deferred behavior in this document or a linked
issue. No issue should implement two of these boundaries implicitly.
