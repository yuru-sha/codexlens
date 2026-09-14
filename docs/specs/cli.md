# CLI specification

This document is the executable command contract for the product described in
[`product.md`](product.md). The existing Rust module names are not part of the
user contract.

## Global options

Every analysis, view, doctor, and optimize command accepts:

```text
--codex-home PATH       default: CODEX_HOME, otherwise platform default
--store PATH            default: ${XDG_STATE_HOME:-~/.local/state}/codexlens/codexlens.db
--scope global|project|project:PATH
--include-archived
--include-subagents
--frozen
--format table|markdown|json    default: table
```

`--scope project` means all known projects. `project:PATH` is normalized to an
absolute project root before matching. A scope filter never changes the
ingestion set; it changes only rendered rows and rankings.

The default store directory is created with owner-only permissions. An
explicit relative `--store` is allowed for tests and deliberate project-local
use. `--codex-home` must be an absolute directory.

`refresh` and `monitor` are explicit ingestion workflows with their own
command-specific options. They do not accept the reporting-only options above
such as `--scope`, `--frozen`, or `--format`; they keep their operational
progress output in the default human-readable form.

## Pipeline

```text
codexlens analyze [options]
codexlens <view> [options]
codexlens sql [SQL] [--store PATH] [--format table|markdown|json]
codexlens query [SQL] [--store PATH] [--format table|markdown|json]  # compatibility alias
codexlens optimize [options] [--print]
```

`analyze` reads Codex inputs read-only and incrementally replaces changed
derived rows. Reporting views consume the existing derived store and never
refresh it; run `analyze` or `refresh` explicitly before reporting new input.
`--frozen` makes that store-only boundary explicit.
`sql` and `query` are the only commands that never create or refresh a store;
they open an existing store read-only. Refresh chatter goes to stderr. JSON
stdout is one JSON document and contains no progress text.

`sql` accepts one positional SQL statement or reads it from stdin when the
argument is omitted. It prepares exactly one statement and rejects writes,
mutable PRAGMA statements, and bounds output at 50 columns and 50 rows. Its JSON
`data` shape is `{columns, rows, omitted_column_count, omitted_count}`; text
and column names are bounded, and blobs are represented by their byte count.
`query` keeps the same read-only behavior and result shape as an explicit
compatibility alias.

## View contracts

| View | Required first section | Row identity | Required action/interpretation |
| --- | --- | --- | --- |
| `doctor` | `WHAT TO FIX FIRST` | opportunity id | problem, impact, target, fix, evidence |
| `inventory` | `CONFIGURATION INVENTORY` | owner + scope + kind + path | usage state and startup/on-demand estimate |
| `overhead` | `CONTEXT COST` | scope + project | observed minimum session-start context and residual |
| `usage` | `WHERE EFFORT GOES` | scope + kind + name | tool, Skill, model, prompt, subagent, and surface counts/tokens/duration/coverage |
| `waste` | `OPPORTUNITIES` | opportunity id | ranked concrete remove/slim/re-scope/fix action |
| `failures` | `RECURRING FAILURES` | category + tool | normalized cause, owner, examples, fix |
| `stuck` | `STUCK WORK` | project + session + path | edit burst/loop sequence and next investigation |
| `prompts` | `HOW YOU STEER CODEX` | scope + prompt class | steer/correct/question/instruct counts, implication, evidence |
| `sessions` | `SESSIONS` | session id | bounded metadata and selection coverage |

`doctor` renders at most 5 opportunities per scope and at most 3 evidence
examples per opportunity. Empty sections are omitted. `inventory`, `usage`,
and `sessions` render at most 50 rows by default and state the omitted count.
JSON contains the complete bounded result set selected by the same limits.
Every actionable doctor item also exposes its owner, target, occurrence/session
counts, bounded evidence, action, limitations, and a focused follow-up command.
`LOOKS HEALTHY` is emitted only for complete observed coverage with no
actionable opportunities and no unknown selected cost/use estimates; empty or
partial coverage and unknown cost/use remain explicitly inconclusive.

## Stable JSON envelope

Every view returns:

```json
{
  "schema_version": 1,
  "command": "doctor",
  "scope": {"kind": "all"},
  "coverage": {
    "session_count": 0,
    "included_session_count": 0,
    "archived_included": false,
    "subagents_included": false,
    "activity_start": null,
    "activity_end": null,
    "limitations": [],
    "limitations_omitted": 0
  },
  "data": {}
}
```

View-specific `data` must use named fields, not rendered text. A finding row
must include `id`, `title`, `scope`, `target`, `impact`, `confidence`,
`occurrences`, `distinct_sessions`, `action`, `evidence`, and `limitations`.
Paths and excerpts are bounded and redacted before serialization.

Coverage limitations are reported as bounded metadata with source provenance,
selected session and record counts, and the affected lens names. They do not
create findings from unknown evidence.

## `optimize`

`optimize` is read-only until the user approves a concrete plan. It receives
the same scoped findings as `doctor`, inspects the named configuration target,
and emits a plan containing exact target files, before/after snippets, reason,
evidence, and a verification step. `--print` prints the briefing; otherwise
the implementation may launch the configured Codex CLI only through a private
temporary file. No mutating write is allowed without explicit confirmation.
The `--print` briefing has the same action semantics in table, Markdown, and
JSON: findings, configuration waste, user-controlled or unknown overhead,
reviewable proposals, explicit skips/limitations, and the exact next workflow.
An actionable non-shell failure is retained even when no canonical shell
command can be inferred; it is never turned into a shell prerequisite.

## cclens compatibility and bounded result matrix

The command family follows the public cclens command enum and routing in
[`src/cli.rs`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L47-L223),
while Codex-specific parsing remains behind codexlens' adapter. The briefing
shape is checked against cclens' optimization renderer in
[`src/core/optimize.rs`](https://github.com/lambdalisue/cclens/blob/main/src/core/optimize.rs#L219-L340)
and its doctor/optimize operational guidance.

| Command | Codex input and scope | Non-empty result | Empty or partial result | Evidence/privacy bound |
| --- | --- | --- | --- | --- |
| `analyze` | selected canonical store; global/project findings | all deterministic findings with counts and targets | empty groups or explicit coverage limitations | source refs and bounded/redacted excerpts |
| `usage` | tool calls/results, token usage, sessions, surfaces; global/project | tool, Skill, model, prompt, subagent, and surface effort signals | no rows or partial usage coverage; unknown is not zero | bounded rows/evidence; no raw payload |
| `inventory` | discovered configured surfaces and observed attribution | kind, owner/scope, path, load mode, use state, estimate, action | unknown use is shown as unknown; no remove claim | bounded paths/evidence; names only |
| `waste` | inventory plus recurring failure/stuck opportunities | ranked remove/slim/re-scope/fix actions | no actionable opportunities, or explicit unknown limitations | max 50 rows and 3 evidence examples |
| `overhead` | always-on surfaces plus session-start snapshots | readable startup, residual, and user-control classification | unknown cost when a snapshot/estimate is missing | bytes and source refs only |
| `prompts` | user messages classified by bounded markers | scope-specific steer/correct/question/instruct implications | no user prompts or partial source coverage | at most 3 bounded excerpts per row |
| `failures` | structured/fallback tool outcomes; scope owner | normalized category/tool, count, examples, suggested fix | no recurring non-transient failure; opaque wrappers stay out | no renderer payload wholesale |
| `stuck` | file operations and short failure/edit windows | project/session/path sequence and next action | no qualifying loop or explicit incomplete evidence | bounded sequence, paths, examples |
| `doctor` | selected findings and view opportunities | highest-impact problem, impact, owner, target, action, follow-up | healthy only when coverage is observed and complete; otherwise inconclusive | max 5 per scope, 3 evidence examples |
| `sql` | existing derived SQLite store and one read-only statement | bounded table/Markdown/JSON rows | missing store or empty result is explicit | max 50 columns/rows; query text is not echoed |
| `optimize` | selected findings, surfaces, and validated instruction baselines | root-cause briefing plus reviewable diff | every unsupported/ambiguous item remains a bounded skip | max 50 proposal rows; no raw inputs; each diff JSON max 16 KiB |

All rows retain global and project scope separately. JSON and Markdown are
renderings of these same bounded action fields, not alternate analyses.

## Real-history smoke procedure

Real history is an explicitly selected local source, not a committed fixture.
After confirming the source scope, run:

```bash
codexlens refresh --codex-home /absolute/path/to/codex-home --store /tmp/codexlens-smoke.sqlite
codexlens analyze --store /tmp/codexlens-smoke.sqlite --frozen --format json > /tmp/codexlens-analyze.json
codexlens doctor --store /tmp/codexlens-smoke.sqlite --frozen
codexlens optimize --print --store /tmp/codexlens-smoke.sqlite --frozen
codexlens sql --store /tmp/codexlens-smoke.sqlite --format json \
  'SELECT COUNT(*) AS sessions FROM sessions'
```

Record store freshness, coverage status, non-empty matching views, bounded
evidence, global/project routing, and the exact skipped limitations. Compare
raw input hashes before/after; do not use `--apply` in the smoke procedure.

## Acceptance tests

CLI tests must cover first-run explicit analysis, default store location, scope
filtering, archive/sub-agent selection, bounded human output, JSON purity,
read-only query behavior, and an end-to-end optimize plan over a synthetic
configuration and two synthetic sessions.

The explicit `refresh` and `monitor` ingestion workflows are tested with their
command-specific option sets; reporting-only options are not part of those
commands.
