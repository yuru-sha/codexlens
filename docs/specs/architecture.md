# Architecture specification

Status: current architecture for the `codexlens` MVP.

## 1. Product boundary

`codexlens` is a local analyzer and improvement advisor for Codex-based
development workflows. It uses session evidence and the instruction context
that was available to a session to find repeated friction, then presents a
scoped proposal for improving `AGENTS.md` or nearby project documentation.

The product is an evidence tool, not a replacement for Codex, a hosted
analytics service, or a general-purpose agent-log platform.

The current binary is a read-only reporting surface over an existing derived
SQLite store. It does not ingest or refresh raw rollout/state inputs, and it
does not apply advisor proposals. The supported command surface and examples
are documented in the [README](../../README.md).

## 2. Goals

- Read Codex local state without modifying it.
- Normalize changing upstream formats into a small stable domain model.
- Preserve unknown valid rollout records and their source location.
- Store enough history in SQLite for incremental analysis.
- Compare observed failures, corrections, rework, verification, and repeated
  knowledge with effective instruction snapshots.
- Produce deterministic, explainable findings with source evidence.
- Keep all MVP processing local and rule-based.

## 3. Non-goals for the MVP

- Sending prompts, code, or reports to a remote service.
- Requiring an LLM or making semantic claims that cannot be traced to evidence.
- Editing `AGENTS.md`, `config.toml`, source files, or rollout files.
- Billing or quota accounting.
- Live monitoring of a running Codex process.
- Supporting every historical or future Codex event before it is observed.
- Reusing implementation code from
  [`cclens`](https://github.com/lambdalisue/cclens) or
  [`codex-session-insights`](https://github.com/cosformula/codex-session-insights).

## 4. Data flow

```text
Codex local state + project instructions
        │
        ▼
      adapters
        │  raw format ends here
        ▼
   canonical records
        │
        ├──────────────► SQLite store
        │                       │
        ▼                       ▼
  diagnostics             lenses and joins
                                │
                                ▼
                             findings
                                │
                         doctor / optimize
```

The store is the boundary between ingestion and reporting:

- adapters read files and map them to canonical records;
- storage persists canonical facts and provenance;
- lenses read the store and emit findings;
- reports render findings without reopening raw inputs.

Incremental source identity is the canonical path plus byte length, modified
time when available, and a streaming FNV-1a fingerprint. An unchanged identity
is skipped. A changed identity replaces only that source's derived rows inside
one transaction; a failed replacement rolls back the deletion and preserves
the previous successful ingest.

When discovered inputs include plain rollouts, state databases provide session
metadata for rollout normalization and are not stored as separate session rows;
this keeps one canonical stored session per rollout source. Direct state-only
ingestion still persists state sessions when explicitly requested.

Reporting commands consume the existing derived store in read-only mode. They
do not reopen raw rollout/state inputs or refresh the store. Analysis,
`sessions`, and `doctor` reports make the recorded freshness state visible;
`optimize --diff` reports proposal and diff state instead. A future `--frozen`
mode is deferred until a refresh workflow exists.

## 5. Components

### Adapter

The adapter owns:

- `CODEX_HOME` and file discovery;
- plain JSONL reading; compressed rollout readers are deferred;
- `state_*.sqlite` thread metadata;
- rollout envelope and event-shape decoding;
- `AGENTS.md`/override discovery;
- `config.toml` settings needed for instruction discovery;
- source path and line provenance.

No raw Codex field name should be required by a lens or a report.

### Canonical model

The domain model owns stable concepts:

- `Session`: identity, timestamps, cwd, project, model/provider metadata;
- `Turn`: turn identity and lifecycle;
- `Record`: timestamped session evidence with a stable kind;
- `Message`: user or assistant text, with source reference;
- `ToolCall` and `ToolResult`: call identity, tool, input summary, outcome;
- `FileOperation`: normalized path and operation when observable;
- `TokenUsage`: a snapshot, not a billing ledger;
- `InstructionSnapshot`: the instruction chain observed or reconstructed;
- `ParseDiagnostic`: source location and a bounded reason;
- `UnknownRecord`: raw valid JSON plus source provenance.

Canonical records may have missing optional fields. Missing data is represented
as unknown, not guessed.

### Store

The SQLite store is a local derived history. The initial schema is expected to
contain these logical areas:

| Area | Purpose |
| --- | --- |
| `sessions`, `turns`, `records` | stable session timeline and provenance |
| `messages` | bounded text needed for corrections and knowledge candidates |
| `tool_calls`, `tool_results` | commands, tools, outcomes, and correlation |
| `file_operations` | changed paths and operation timestamps |
| `token_usage` | deduplicated usage snapshots |
| `instruction_files` | discovered instruction sources and scope |
| `instruction_snapshots` | historical effective-chain content/hash |
| `corrections` | detected user corrections and evidence |
| `findings` | reproducible analysis results |
| `ingested_files` | incremental-ingest identity and diagnostics |
| `schema_versions` | store migration state |

Unknown records are kept in `records` (or a dedicated table) as raw JSON in
the local store only. Reports do not print them unless explicitly requested.

The store must use a schema version and migrations. Deleting the store is not
an upgrade strategy because it destroys historical evidence.

### Lenses

Each lens is a pure transformation from canonical/store facts to findings.
The MVP lenses are:

- `failures`;
- `corrections`;
- `rework`/`stuck`;
- `verification`;
- `knowledge`/`rediscovery`;
- `instructions`.

Their contracts and conservative heuristics are defined in
[`analysis.md`](analysis.md).

### Advisor and report

`doctor` is the compact aggregate view. `optimize --diff` groups findings into
review-only candidate changes and renders unified diffs without writing the
target files. The other reporting commands select one lens or list stored
sessions; `analyze` selects all lenses. `rework`/`stuck` and
`knowledge`/`rediscovery` are command aliases.

MVP output must include:

- finding type and severity;
- confidence and the heuristic used;
- affected scope/path;
- counts and distinct session count;
- links to local source path and line where available;
- a suggested action that is explicitly a proposal.

`optimize --apply` is out of scope until a separate issue defines backup,
patch validation, scope checks, recovery behavior, and explicit confirmation.

## 6. Instruction resolution

The implementation must model the Codex instruction chain, not only the
current root `AGENTS.md`:

1. Global scope: under `$CODEX_HOME`, use the first non-empty
   `AGENTS.override.md`, otherwise `AGENTS.md`.
2. Project scope: from the project root to the session cwd, inspect each
   directory in order: `AGENTS.override.md`, `AGENTS.md`, then configured
   fallback filenames. Include at most one file per directory.
3. Merge root-to-cwd with blank-line boundaries; later, deeper guidance has
   precedence.
4. Apply `project_doc_max_bytes` cumulatively to the project chain only,
   starting at the project root. Global guidance is loaded independently and
   is not charged against that project budget.

The resolver must record:

- source path and scope;
- whether the source was an override or fallback;
- content hash and byte count;
- chain order;
- effective-chain hash;
- whether the snapshot came from the rollout, the filesystem at ingest time,
  or was unavailable.

Rollout-provided instruction text is marked observed; a filesystem chain is
marked reconstructed at ingest time; unavailable snapshots carry no content or
hash and are inconclusive for later comparisons.

Historical analysis must never silently compare an old session with only
today's files. If an exact historical snapshot is unavailable, the finding
must say so.

The instruction-related `config.toml` subset is intentionally small:
`project_doc_fallback_filenames` defaults to an empty list and
`project_doc_max_bytes` defaults to 32 KiB. An explicit config path wins over
`$CODEX_HOME/config.toml`; malformed or unreadable input keeps these defaults
and produces a bounded diagnostic. Unknown settings are ignored.

When a rollout and state index provide the same session, rollout
`session_meta` values are authoritative because they belong to the event
stream. State values fill missing fields; differing non-empty values are kept
as a diagnostic and are not silently overwritten.

The resolution rules follow the
[official Codex AGENTS.md guide](https://developers.openai.com/codex/guides/agents-md).

## 7. Privacy and safety

- All source reads are read-only.
- No network is needed for analysis.
- Raw source content stays in the user's local store and is never copied into
  this repository.
- Human-readable reports should bound or redact prompt, command, and output
  excerpts.
- Do not include raw session data in tests, issues, PRs, or bug reports.
- A source file that cannot be read produces a diagnostic; it must not cause
  unrelated sources to be silently skipped.

## 8. Compatibility and evolution

The adapter must:

- ignore unknown fields;
- keep an unknown record instead of dropping it;
- tolerate absent optional fields;
- distinguish malformed JSON from a valid but unknown event;
- report the source file and line for every parse diagnostic;
- avoid assuming one event subtype is the complete history.

The rollout source is an upstream-owned format. Relevant current shapes are
tracked in [`session-format.md`](session-format.md), with the upstream
implementation available in the
[Codex rollout module](https://github.com/openai/codex/tree/main/codex-rs/rollout).

## 9. Delivery order

1. Foundation: CLI, specifications, CI, and synthetic fixtures.
2. Codex ingestion: discovery, rollout/state adapters, normalization, and
   incremental storage.
3. Instructions: resolver, config settings, and effective snapshots.
4. Lenses: deterministic findings over stored evidence.
5. Advisor: `doctor`, proposal generation, and `optimize --diff`.

Phases 0 through 4, including the reporting command integration, are complete
for the MVP. Compressed readers, `--frozen`, machine-readable output, live
monitoring, and `optimize --apply` remain deliberately deferred.

Every phase must leave the repository buildable and its behavior covered by
focused deterministic tests.
