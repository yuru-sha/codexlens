# Post-MVP input and reporting contracts

Status: compatibility and privacy contract for the implemented Phase 5
boundaries, plus an entry contract for future feature issues. The compressed
rollout reader in section 1, refresh/frozen reporting in section 2,
machine-readable output in section 3, live monitoring in section 4, and safe
apply in section 5 are implemented by issues #57, #58, #59, #60, and #61.
Issue #82 adds the shared read-only reporting coverage metadata in section 3.
Issue #83 adds explicit reporting-period selection in section 7.

Issue #53 originally tracked the capabilities that crossed the MVP input,
runtime, output, or write boundary. Issues #57 through #61 implement sections
1 through 5. Future feature issues must state which current boundary they
extend, add the relevant compatibility and privacy tests, and pass the
relevant safety checks before changing the command surface.
The positive tests below guard the implemented capabilities and future
extensions of their boundaries.

## Shared boundary

Every capability keeps the existing flow:

```text
upstream input -> adapter -> canonical records -> derived store -> lens/report
```

- Upstream field names stop at the adapter.
- Raw rollout/state inputs and application source files are source read-only:
  refreshes and reports may read them but never rewrite, rename, truncate,
  repair, or delete them. The only write exception is the explicit,
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

## Current regression coverage

The executable guards for the implemented boundaries and their future
extensions are kept explicit:

- `compressed_rollout_input_is_ingested_incrementally_and_read_only` and
  `corrupt_compressed_rollout_does_not_block_valid_sibling` cover compressed
  reader parity, bounded corruption handling, incremental replacement, and
  unchanged sources.
- `refresh_and_frozen_reporting_are_explicit_and_read_only` checks the refresh
  boundary independently; rejected errors stay bounded and stores stay
  unchanged.
- `monitor_command_updates_a_local_store_and_honors_max_polls`, together with
  the monitor integration tests, covers partial lines, finite replay,
  restart, rotation/truncation, duplicate identities, state fingerprints, and
  deterministic stop timing.
- `machine_readable_output_is_versioned_deterministic_and_canonical` and
  `optimize_json_contains_typed_proposals_and_keeps_skips_in_document` cover
  the implemented JSON command shapes and canonical aliases.
- `optimize_apply_requires_explicit_confirmation` checks the implemented write
  boundary; rejected confirmation stays bounded and the store stays unchanged.
- `reporting_is_deterministic_bounded_and_does_not_refresh_or_write` checks
  repeated human-readable output, bounded/redacted evidence, and unchanged
  derived/raw files.
- `reporting_commands_render_local_store_data` and
  `optimize_diff_renders_a_proposal_without_writing_the_target` cover the
  current deterministic human-readable, bounded-evidence, and read-only
  reporting behavior.
- `reporting_metadata_exposes_store_coverage_and_separates_ingestion_time` and
  `empty_reporting_store_marks_activity_unknown_without_using_ingestion_time`
  cover multi-session spans, explicit empty/partial coverage, missing and
  invalid activity timestamps, distinct ingestion time, and deterministic JSON
  metadata.
- `reporting_period_filter_is_half_open_and_visible_in_human_and_json`,
  `empty_reporting_period_has_no_selected_activity`,
  `reporting_period_rejects_invalid_and_reversed_bounds`,
  `reporting_coverage_marks_unknown_event_timestamps_as_partial`, and
  `all_read_only_reports_share_period_selection_and_json_coverage` cover
  normalized bounds, empty intervals, coverage metadata, aliases, and the
  `optimize --apply` boundary.

The executable tests cover the primary compatibility and privacy boundaries
described below. The remaining cases are contract requirements for future
regression coverage. Sections 1 through 5 and the explicit reporting-period
selection in section 7 are implemented; section 6 remains a planning contract.

## 1. Compressed rollout readers

Implementation status: implemented by Issue #57.

### Scope

The adapter's compressed rollout reader yields the same logical `(line number,
bytes)` stream for a `.jsonl.zst` source as for its equivalent plain `.jsonl`
source, then reuses the existing JSONL parser, normalizer, and store
transaction. Compression details do not appear in canonical records, lenses,
or reports.

The canonical source identity remains the canonical source path. The
compressed source bytes provide the compressed-byte fingerprint used for change
detection. A changed compressed source is replaced as one derived-source
transaction; an unchanged source is skipped. Decompression must retain the
existing maximum-line bound after decompression and must not allocate an
unbounded line.

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

Implementation status: implemented by Issue #58.

### Scope

The explicit refresh workflow builds or updates the derived store from
discovered inputs. Refresh reads raw rollout/state and instruction sources,
applies the existing identity and incremental-ingest rules, and commits all
replacements atomically. A failed source read or transaction leaves the
previous successful derived state available.

Reporting remains a separate operation over the derived store. The explicit
`--frozen` reporting mode means "use exactly this store": it must not discover
or reopen raw inputs, refresh the store, or silently claim that the store is
current. Missing or invalid stores remain bounded errors; recorded freshness
is shown in the report.

Issue #58 implements `refresh` with `--store`, `--codex-home`/`--home`,
`--include-archived`, and `--config` input options. All supported reporting
commands accept `--frozen`; both frozen and default reporting read only the
selected derived store, and recorded freshness is reported without claiming
that the store is current. The explicit refresh path is the only workflow that
discovers or ingests raw inputs; reporting does not refresh implicitly.

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
- Frozen and normal finding/session reports contain only derived-store data and
  permitted bounded evidence; they do not transmit data or read outside the
  configured input boundary. `optimize --diff` is the explicit read-only
  exception: it may read the validated target instruction files needed to
  render a proposal, as defined by the advisor contract.
- Both workflows preserve the source read-only guarantee, including on parse,
  migration, and rollback errors.

## 3. Machine-readable output

Implementation status: implemented by Issues #59 and #82.

This section is implemented for the supported reporting commands by issue
#59 and the coverage extension in #82. The default human-readable output
remains compatible; #82 adds the coverage metadata prefix described below.

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
  groups}` with optional additive `coverage`. `freshness` is
  `{state, source_count, latest_ingested_at}`;
  `groups` is an ordered array of `{scope, findings}`; each finding contains
  the typed `Finding` fields `kind`, `severity`, `confidence`, `scope`, `key`,
  `summary`, `evidence`, `occurrences`, `distinct_sessions`,
  `affected_paths`, `observed_commands`, `sequence`, `suggested_action`,
  `limitations`, and `verification_status`, plus `heuristic`.
- `sessions` uses `{freshness, sessions}` with optional additive `coverage`,
  where each session is `{id, created_at, updated_at, cwd, project}`.
- `optimize --diff` uses `{rendered, skipped}`, where `rendered` contains the
  typed proposal and unified `diff`, and `skipped` contains
  `{target_path, reason, proposal}`. `proposal` is the typed proposal when a
  rendered diff was omitted for machine-output safety, and is `null` when the
  proposal was already skipped before diff rendering.

Machine-readable diffs are redacted before output and are emitted only when
the complete, unchanged diff is at most 16 KiB. A rendered proposal requiring
redaction or exceeding the limit is moved to `skipped` with a bounded reason;
its bounded typed proposal and evidence remain available there. JSON never
emits a syntactically truncated or redaction-altered unified diff.

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
and `groups: FindingGroup[]`, plus optional additive `coverage: Coverage`.
`Freshness` has required
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

The `sessions` data object is `{freshness: Freshness, optional coverage:
Coverage, sessions: Session[]}`;
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
`{target_path: string, reason: string, proposal: Proposal | null}`.
`rendered` and `skipped` are arrays of those exact element types. When a
rendered proposal is omitted for machine-output safety, `proposal` preserves
its bounded scope and evidence references so the machine report remains
comparable with the human proposal summary.

### Reporting coverage metadata

Issue #82 adds the same additive `coverage` object to `sessions` and finding
reports (the shared finding renderer also exposes it for focused lens
commands). The JSON schema remains version `1`: existing required fields,
freshness fields, command names, and aliases keep their meaning, while
`coverage` is an optional object that current producers always emit. Existing
readers must continue ignoring unknown optional fields; a future change to the
meaning or type of an existing field still requires a new schema version.

`Coverage` is:

```text
{
  "scope": "selected_store",
  "status": "empty | observed | partial",
  "activity_start": "timestamp | null",
  "activity_end": "timestamp | null",
  "valid_activity_timestamps": non-negative integer,
  "missing_activity_timestamps": non-negative integer,
  "invalid_activity_timestamps": non-negative integer,
  "session_count": non-negative integer,
  "record_count": non-negative integer
}
```

The report covers exactly the selected derived store. It does not claim to
cover all historical activity or currently available raw inputs. `refresh` is
the only operation that discovers or ingests raw inputs; reporting never
refreshes implicitly, and archived sessions are included only when refresh is
run with `--include-archived`.

`session_count` is the number of distinct session IDs observed across the
canonical session-bearing data. `record_count` is the number of canonical
`Record` rows, including unknown record kinds. Activity timestamp counts are
field observations, not distinct instants: they include session
`created_at`/`updated_at`, turn start/completion and lifecycle timestamps, and
the canonical record, message, file-operation, and token-usage timestamps.
Missing fields increment `missing_activity_timestamps`; present values that do
not pass the existing timestamp parser increment `invalid_activity_timestamps`.
`activity_start` and `activity_end` use only valid observed activity times and
are `null` when none exist. The latest recorded ingestion time remains only in
`freshness.latest_ingested_at`; it is never substituted for an unknown activity
time. `empty` means there are no canonical sessions, records, or timestamp
observations; `partial` means at least one timestamp observation is missing or
invalid; otherwise the status is `observed`. The `sessions` list uses the same
session-ID set as `session_count`; an ID inferred from canonical records or
other session-bearing rows has `null` metadata when no session row exists.

For `optimize --diff`, `action` is `add | modify | remove | move_to_docs |
split_scope`; all fields are required, including nullable fields, as defined
by `Proposal` above.

`groups`, `findings`, `evidence`, `affected_paths`, `observed_commands`,
`sequence`, `limitations`, `sessions`, `rendered`, and `skipped` are arrays;
`finding_counts` is an object from finding-kind strings to non-negative
integers. `finding_counts` may omit zero-valued kinds. Arrays are ordered as
documented below and never omitted when empty.

The future human-readable serializer contract is line-oriented UTF-8 with LF line endings,
no ANSI control sequences, and exactly one final LF. Its grammar is:

```text
Analyzed period: <unknown | timestamp | timestamp .. timestamp>\n
Coverage: selected store (<empty | observed | partial>; bounded scope note)\n
Activity: <unknown | timestamp | timestamp .. timestamp>\n
Activity timestamps: <integer> valid, <integer> missing, <integer> invalid\n
Sessions: <non-negative integer>\n
Records: <non-negative integer>\n
Latest ingestion: <unknown | timestamp>\n
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
line to stderr. The existing [`analysis.md`](analysis.md) and README command
table remain authoritative for the current command list and examples.

This is a target contract for a future human-readable serializer, not a
retroactive claim that every current renderer already escapes multiline values.
The existing MVP human renderers remain the compatibility baseline until that
serializer is implemented. For the future serializer, bounded text is
redacted before rendering and multiline values must be escaped rather than
creating extra grammar lines.

The command-specific empty and alias forms are exact:

- Finding commands (`analyze`, each focused lens, `doctor`, and their aliases)
  emit the coverage metadata lines above and `Finding counts: none` when there
  are no groups.
- `sessions` emits the coverage metadata lines above and, for each session in
  lexicographic `id` order, `- <id>` followed by exactly
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

Implementation status: implemented by Issue #60.

### Scope

Live monitoring is an explicit local runtime boundary. It observes append-only
rollout/state changes and produces incremental findings while reusing the
adapter and canonical model; it does not create a second parser or leak
upstream event names into lenses.

The implementation defines the input lifecycle: source identity and offsets,
incomplete final lines, rotation/truncation, duplicate events, stop behavior,
restart behavior, and the distinction between partial observations and a
completed session. A monitor may not silently turn a partial observation into a
final finding.

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

Issue #60 implements this section with the explicit local `monitor` command,
bounded rollout cursors, state fingerprints, and the existing adapter and
canonical normalization flow.

### Implementation decisions for issue #60

- `monitor --source PATH --kind rollout|state` is the explicit local runtime
  boundary. `--max-polls` provides a deterministic finite stop boundary for
  automation; without it the command continues polling locally. `--cursor PATH`
  persists the bounded cursor at a clean stop and reloads it on the next
  invocation; the cursor path must not alias the source or derived store.
- A rollout cursor records the canonical source path, the byte offset after the
  last complete newline, the physical line and canonical sequence counts, a
  bounded FNV-1a prefix digest, and a bounded recent identity window. The
  monitor passes only complete newline-terminated bytes to the existing JSONL
  adapter, so an incomplete final line remains source data and is retried on
  the next poll.
- A source smaller than the recorded offset is a `truncated` transition. A
  changed prefix at the recorded offset is a `rotated` transition. Both reset
  the cursor and replace that source's derived rows atomically before reading
  the new complete prefix; neither transition silently appends old and new
  generations together.
- Explicit `event_id`, `record_id`, or `id` values are checked in a bounded
  recent window. A repeated identity is reported as a diagnostic and skipped;
  identities older than the configured window are treated as new events.
- State databases have no line offset. The monitor fingerprints the complete
  read-only source and reuses the existing state adapter only when that
  fingerprint changes. Rollout batches append canonical rows to the derived
  store using the existing normalizer, while session, turn, pending tool-call
  candidates (including calls without an ID), and recent tool-result
  correlation context are carried as bounded canonical state rather than raw
  payloads. Call/result-derived file operations use the same bounded call
  identity to avoid double counting across poll boundaries. If a later failure
  invalidates a provisional call-derived operation, the append transaction
  retracts that operation so live and batch ingestion remain equivalent.

## 5. `optimize --apply`

Implementation status: implemented by Issue #61. The contract below remains
the compatibility and privacy boundary for the command.

### Scope

Keep `optimize --diff` review-only and read-only. `optimize --apply`
may write only the validated proposal write set in the allowed instruction
scope. For `add`, `modify`, and `remove`, the write set is `target_path`; for
`move_to_docs` and `split_scope`, it is both `source_path` and `target_path`.
It must never write rollout files, state databases, or the derived store as a
side effect of applying a proposal.

The allowed roots and file classes come from the resolved instruction scope,
not from either proposal path alone:

- Global scope permits only the canonical configured `$CODEX_HOME` root and
  the selected global instruction path (`AGENTS.override.md` or `AGENTS.md`)
  from the effective instruction resolution.
- Project and instruction scopes permit only the canonical resolved project
  root and the selected instruction paths from the effective instruction
  resolution. A `move_to_docs` target may also be an existing Markdown
  documentation file under that project root (`docs/` or `README.md`); other
  file classes are rejected.
- Every instruction-path write must match the canonical selected-path set for
  the proposal's effective instruction resolution. An unavailable or ambiguous
  resolution, or an unselected same-name file, is rejected; filename matching
  alone is insufficient.
- Every write-set path must be an existing regular file with no symlink
  component. Raw `..` traversal is rejected before canonicalization; every
  component is canonicalized and a result outside its allowed root is
  rejected. Missing or ambiguous roots and `Path` scope are rejected.
- For `move_to_docs` and `split_scope`, source and target must be distinct
  canonical paths under the same resolved root; source must be a selected
  instruction file and target must be a permitted instruction or documentation
  file. The source_path field is not an independent authority to expand the
  scope.

Before any write, the implementation must:

1. require an explicit confirmation step; interactive use confirms the exact
   validated write set, while non-interactive use must provide `--yes` after
   the diff was reviewed;
2. re-read every file in the validated write set and verify its expected
   content hash;
3. validate the generated patch against that exact write set, with no fuzzy or
   partial application;
4. validate every path in the write set for scope, regular-file status, and
   symlink/path boundaries;
5. create recoverable backups for every file in the write set before the first
   write.

Writes are atomic across each proposal write set. A single `--apply` invocation
is one transaction over the complete proposal batch: no proposal write set may
remain committed if a later proposal fails. If any write in the workflow fails,
it must restore every file changed in any prior or current write set from the
backups, report recovery status, and return failure. Backups remain available
after a successful run; the initial implementation must not delete them
implicitly. A separate, explicit cleanup policy may be specified later. A
successful result must identify the files changed and the backup/recovery
outcome.

### Compatibility tests

- An out-of-scope source path, raw `..` traversal, symlink component,
  non-regular file, or missing/ambiguous root rejects the whole proposal batch
  before any write.
- Missing confirmation, a changed target or source hash, an invalid patch, or
  a scope violation performs no write to any file in the validated write set
  and no partial apply.
- A synthetic multi-proposal failure after an earlier proposal was written
  restores every file in every changed write set and leaves backups available
  for inspection.
- Move and split proposals re-read, hash-check, back up, and roll back both
  source and target paths.
- A successful apply changes only the expected bytes in each validated write
  set; `--diff` remains byte-for-byte read-only and continues to render the
  same proposal.
- Rollout/state files and the derived store are byte-for-byte unchanged by
  both successful and failed apply attempts.

### Privacy tests

- Confirmation, success, failure, backup, and recovery messages contain
  bounded paths and summaries, never raw session prompts, commands, outputs,
  tokens, or credentials.
- Backups are local, scoped to the validated write set, and are never uploaded
  or copied into repository fixtures.
- Recovery failures are explicit and actionable; the command never reports
  success while any file or backup is in an unknown state.

## 6. Scoped finding evaluation

Status: planning contract for Issue #84. This section defines how a bounded
human-reviewed local pilot may evaluate findings and proposals; it does not
claim that a real-history pilot has been run or add an automated analytics
backend.

### Scope

Before reading real history, the owner must select and authorize the
source/project scope, observation period, archive inclusion, storage location,
and retention/deletion policy. The pilot records the exact code version,
relevant settings, resolved interval, observed coverage and sample counts, and
store freshness. It reviews bounded samples by lens and severity, records
`actionable`, `incorrect`, or `inconclusive` judgments with denominators, and
checks a small independent activity sample for missed problems.

`optimize --diff` is the proposal-evaluation boundary. It remains review-only;
optimize --apply requires separate review and authorization. Before-and-after
comparisons use the documented comparable windows and normalized denominators,
and report observed associations without causal claims. Results contain only
concise aggregates, limitations, prioritized decisions, and synthetic examples.

The executable procedure is the [finding usefulness pilot runbook](../evaluations/finding-usefulness-pilot.md).
Until the coverage and period contracts in #82 and #83 are available, any real
run must disclose unfiltered selected-store coverage and must not claim a
comparable period.

### Compatibility tests

- The runbook blocks a real read until all owner authorization fields are
  recorded and keeps the selected store and refresh/frozen boundary explicit.
- The sample worksheet records population and review denominators separately,
  includes all three usefulness judgments, and includes an independent
  activity sample.
- Proposal review uses `optimize --diff` and records skipped proposals and
  scope accuracy; no evaluation step invokes `optimize --apply`.
- A comparison records the resolved intervals, shared selection semantics, and
  normalized denominators, while keeping association separate from causation.
- A confirmed failure maps to a newly constructed synthetic regression case and
  a narrowly scoped follow-up issue.

### Privacy tests

- Raw logs, excerpts, credentials, personal identifiers, and private paths are
  kept outside committed artifacts and GitHub.
- The derived store and detailed worksheet follow the owner-selected
  retention/deletion policy; reporting remains local and source read-only.
- Published output is bounded aggregate evidence with synthetic examples only,
  and no automatic instruction edit or external analytics service.

## 7. Explicit reporting periods

Implementation status: implemented by Issue #83.

### Scope

The read-only reporting commands `analyze`, `sessions`, `failures`,
`corrections`, `rework`/`stuck`, `verification`, `knowledge`/`rediscovery`,
`instructions`, `doctor`, and `optimize --diff` accept `--since` and `--until`.
Selection applies to the loaded derived store before lens aggregation,
ranking, proposal generation, or rendering. It never refreshes the store or
reopens raw rollout/state inputs. `monitor` keeps its own cursor/ingestion
boundary, and `optimize --apply` rejects period selectors because its validated
write set must not become implicit.

### Timestamp and interval contract

- Each bound is a complete RFC3339 date-time with seconds, an optional
  fractional part of one through nine digits, and either `Z` or a numeric
  `+HH:MM`/`-HH:MM` offset. Naive timestamps and leap-second `:60` values are
  rejected.
- Bounds are normalized to UTC for comparison and output. The interval is
  half-open: `[since, until)`. A missing bound is unbounded; equal bounds are
  valid and select no timestamp; a reversed or malformed bound is an
  actionable error.
- Relative periods are intentionally not accepted. A rolling-window caller
  must resolve one reference instant and pass absolute bounds explicitly.

### Selection and coverage contract

- Canonical records are the primary activity population. A valid record is
  included only when its timestamp is in the interval. A missing or invalid
  record timestamp is excluded from a filtered report and counted as
  `unknown_timestamp_records`; unfiltered reports retain it.
- Sessions and turns with selected facts, or spans intersecting the interval,
  remain available as boundary context. The observed period is derived from
  valid selected timestamps, not from store freshness.
- A selected user message retains its immediately preceding assistant message
  in the same session as context. A selected tool call or result retains its
  matching counterpart for correlation even when the counterpart is outside
  the interval; a counterpart retained only for that purpose is not itself an
  observed verification event.
  Turn completion and lifecycle events outside the interval are removed from a
  filtered turn.
- File operations and token usage use their own event timestamp or canonical
  source-record timestamp. Instruction snapshots use the same rule, and
  instruction joins follow selected sessions.
- Every lens and `optimize --diff` receives the selected canonical data. No
  report aggregates the unfiltered store and applies a display-only filter.

Human-readable filtered reports show the requested interval, observed period,
selected counts, period coverage state, unknown/excluded counts, and store
freshness as separate values. Version-1 JSON keeps the existing report fields
and adds `data.coverage` with the existing selected-store coverage plus
`requested_start`, `requested_end`, `observed_start`, `observed_end`,
`included_sessions`, `included_records`, `excluded_records`,
`unknown_timestamp_records`, `unknown_timestamp_events`, and `state`.
`optimize --diff` also includes its freshness object when filtered. The period
state is `empty` when no valid selected timestamp is observed, `partial` when
unknown timestamps remain, and `complete` otherwise.

### Compatibility tests

- Equivalent UTC and offset bounds produce byte-identical output, boundaries
  obey half-open membership, empty intervals select no activity, and invalid
  or reversed bounds fail without changing the store.
- Synthetic calls/results, boundary-crossing messages/turns, missing and
  invalid timestamps, aliases, all read-only report commands, and the
  `optimize --apply` rejection are covered by deterministic CLI/unit tests.

### Privacy tests

- Period selection reads only the derived store; tests use bounded synthetic
  data and do not commit real prompts, commands, outputs, credentials, or
  personal identifiers.
- Reporting remains local and source read-only; period metadata does not expose
  raw event content or bypass the existing bounded evidence contract.

## Entry gate for implementation issues

Before a future feature issue extends one section or changes a boundary, it
must name that section and record that the relevant acceptance criteria and
boundary are explicitly agreed in the issue or PR before implementation
starts, then add the listed compatibility and privacy tests with synthetic data.
It must
also update the relevant adapter, canonical, store, lens, or report
specification, preserve the source read-only boundary, run the pinned CI
commands, and record any newly deferred behavior in this document or a linked
issue. No issue should implement two of these boundaries implicitly.
