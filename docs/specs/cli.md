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
examples per opportunity. `--limit` lowers the per-scope cap. The JSON
`top_fixes_omitted_count` field and the human report both disclose candidates
removed by the per-scope or overall summary bound. Empty sections are omitted.
`inventory`, `usage`,
and `sessions` render at most 50 rows by default and state the omitted count.
JSON contains the complete bounded result set selected by the same limits.
Every actionable doctor item also exposes its owner, target, occurrence/session
counts, bounded evidence, action, limitations, and a focused follow-up command.
`LOOKS HEALTHY` is emitted only for complete observed coverage with no
actionable opportunities and no unknown selected cost/use estimates; empty or
partial coverage and unknown cost/use remain explicitly inconclusive.

## Stable JSON envelope

Typed views, `doctor`, and `optimize --print` return:

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
  "freshness": {
    "state": "recorded",
    "source_count": 0,
    "latest_ingested_at": null
  },
  "data": {}
}
```

View-specific `data` must use named fields, not rendered text. Actionable
opportunity rows in `doctor`, typed opportunity views, and `optimize --print`
must include `id`, `title`, `scope`, `target`, `impact`, `confidence`,
`occurrences`, `distinct_sessions`, `action`, `evidence`, and `limitations`.
Paths and excerpts are bounded and redacted before serialization.

`analyze` deliberately retains the canonical finding schema rather than
pretending that a finding is already an actionable opportunity. Its finding
rows contain `kind`, `severity`, `confidence`, `scope`, `key`, `summary`,
`evidence`, `occurrences`, `distinct_sessions`, `affected_paths`,
`observed_commands`, `sequence`, `suggested_action`, `limitations`,
`verification_status`, and `heuristic`. `affected_paths` is the available
target evidence and `suggested_action` is the canonical action; `doctor` and
the typed opportunity views resolve those into explicit `target` and `action`
fields. Canonical findings retain at most 12 bounded evidence/path/command
entries; typed opportunities retain at most 3 evidence examples.

`sql` and `query` intentionally use a minimal JSON envelope containing only
`schema_version`, `command`, and their bounded `data` result; they do not
pretend that a query result has analysis freshness or coverage.

Coverage limitations are reported as bounded metadata with source provenance,
selected session and record counts, and the affected lens names. They do not
create findings from unknown evidence.

## `optimize`

`optimize` is read-only until the user approves a concrete plan. It receives
the same scoped findings as `doctor`, inspects the named configuration target,
and emits a plan containing exact target files, before/after snippets, reason,
evidence, and a verification step. Non-instruction targets without a retained
baseline carry bounded review-only metadata instead of before/after snippets.
`--print` prints the briefing; otherwise
the implementation may launch the configured Codex CLI only through a private
temporary file. No mutating write is allowed without explicit confirmation.
The `--print` briefing has the same action semantics in table, Markdown, and
JSON: findings, configuration waste, user-controlled or unknown overhead,
reviewable proposals, explicit skips/limitations, and the exact next workflow.
An actionable non-shell failure is retained even when no canonical shell
command can be inferred; it is never turned into a shell prerequisite.

## cclens behavioral oracle

The user-facing shape is derived from cclens, while Codex-specific parsing
remains behind codexlens' adapter. These are the exact source locations used as
the behavioral oracle:

| Contract area | cclens source location |
| --- | --- |
| command enum, format flags, and dispatch/scope routing | [`src/cli.rs#L47-L223`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L47-L223), [`src/cli.rs#L225-L322`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L225-L322) |
| doctor ordering, empty state, cost, pruning, and healthy state | [`src/cli.rs#L750-L1032`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L750-L1032) |
| stuck, failure, prompt, overhead, analysis, usage, and inventory views | [`src/cli.rs#L1176-L1237`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L1176-L1237), [`src/cli.rs#L1242-L1379`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L1242-L1379), [`src/cli.rs#L1388-L1583`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L1388-L1583), [`src/cli.rs#L1584-L2220`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L1584-L2220) |
| SQL bounds and read-only query behavior | [`src/cli.rs#L703-L746`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L703-L746) |
| finding model, global/project routing, evidence, and action counts | [`src/core/optimize.rs#L11-L122`](https://github.com/lambdalisue/cclens/blob/main/src/core/optimize.rs#L11-L122) |
| local-only optimization, privacy, concrete actions, and follow-up workflow | [`src/core/optimize.rs#L132-L213`](https://github.com/lambdalisue/cclens/blob/main/src/core/optimize.rs#L132-L213), [`src/core/optimize.rs#L215-L340`](https://github.com/lambdalisue/cclens/blob/main/src/core/optimize.rs#L215-L340) |
| waste ranking and its actionable opportunity union | [`src/cli.rs#L2274-L2407`](https://github.com/lambdalisue/cclens/blob/main/src/cli.rs#L2274-L2407) |
| doctor operational contract | [`doctor/SKILL.md#L8-L44`](https://github.com/lambdalisue/cclens/blob/main/plugins/cclens/skills/doctor/SKILL.md#L8-L44) |
| optimize operational contract | [`optimize/SKILL.md#L8-L50`](https://github.com/lambdalisue/cclens/blob/main/plugins/cclens/skills/optimize/SKILL.md#L8-L50) |

### Codex input mapping

The following differences are intentional adapter boundaries, not changes to
the user-facing analysis meaning:

| cclens concept | CodexLens input/option | Preserved equivalence or explicit difference |
| --- | --- | --- |
| Claude transcript records and session database | rollout JSONL plus `state_*.sqlite` normalized into canonical records | tool outcomes, messages, sessions, token usage, file operations, and provenance feed the same lens categories; unknown valid records are retained, not guessed |
| Claude config and project instruction files | global/project `AGENTS.md`, `config.toml`, and discovered Codex surfaces | global/project ownership, bounded instruction evidence, and configuration actions remain separate |
| transcript input root (`--projects`) | `--codex-home` and input discovery | both select the local raw input root; Codex additionally discovers rollout JSONL and `state_*.sqlite` inputs under the selected home |
| scope filter (`--scope`) | `--scope global|project|project:PATH` | `project` means all known projects; `project:PATH` is normalized before matching; scope filters output, not ingestion |
| cclens database/report store | `--store PATH` derived SQLite store | reporting reads the derived store without refreshing raw inputs; `refresh`/`analyze` are the explicit ingestion workflows |
| transcript-derived usage/cost | canonical tool/token records plus surface inventory and startup snapshots | totals are comparable categories, not byte-for-byte Claude measurements; missing attribution or cost remains unknown/partial |
| cclens local doctor/optimize workflow | `doctor` and `optimize --print/--diff/--apply` | local-only, bounded evidence, reviewable targets, and explicit confirmation are preserved; `--apply` remains the only mutating path |

Codex does not claim cclens' renderer or storage internals as input
compatibility. The adapter is the only place allowed to translate raw Codex
field names into these canonical concepts.

Typed views, `doctor`, and `optimize --print` have the same semantics in table
output, Markdown, and JSON. Markdown adds a heading; JSON keeps the stable
envelope above and puts the named rows in `data`. `analyze` uses the canonical
finding envelope described above. `optimize --diff` uses its proposal envelope
and only adds freshness/coverage inside `data` for a filtered period. `sql` and
`query` retain the minimal query envelope.

Ordering is deterministic and part of the contract:

| Output | Required order |
| --- | --- |
| `analyze` findings | severity descending, confidence descending, distinct sessions descending, normalized key ascending, occurrences descending, then kind/key/scope tie-breakers |
| `doctor` | global group before project groups; findings use the `analyze` order; top-fix opportunities use severity descending, confidence descending, distinct sessions descending, occurrences descending, then id ascending; sections are `WHAT TO FIX FIRST`, optional `COST`, optional `CONFIG WORTH PRUNING`, then conditional `LOOKS HEALTHY` |
| `inventory` | scope, kind, path, name, then id ascending |
| `overhead` | global row first, then project rows by normalized project path ascending |
| `usage` | occurrences descending, output tokens descending, kind, name, then scope ascending |
| `prompts` | prompt class order `steer`, `correct`, `question`, `instruct`, then scope ascending |
| `waste`, `failures`, `stuck` | opportunity severity descending, confidence descending, distinct sessions descending, occurrences descending, then id ascending |
| `optimize --print` | sections `FINDINGS`, `CONFIGURATION WASTE`, `OVERHEAD`, `REVIEWABLE PROPOSALS`, then `NEXT WORKFLOW`; each list keeps its source ordering |
| `optimize --diff` | rendered proposals by target path, action, problem; skipped proposals by target path, reason |
| `sql` / `query` | statement result order, with no analysis ranking |

Markdown preserves the same section, row, and field order under one top-level
heading. SQL preserves the statement's row order and applies only the output
bounds.

For actionable rows, `target` identifies the bounded file or configuration
surface to inspect, and `action` is the concrete verb for that target: remove,
slim, re-scope, or fix. `follow_up` is a separate bounded command or
investigation step. Missing or ambiguous evidence leaves the action absent or
review-only rather than inventing a target.

### Named JSON data fields

The following field sets are part of the contract; additional fields may be
added only as optional, bounded fields that older readers can ignore.

| Command | Named `data` fields and row shape |
| --- | --- |
| `analyze` | `period_start`, `period_end`, `session_count`, `freshness`, `finding_counts`, `groups`, and `coverage`; `groups[]` contains `scope` and `findings[]` using the canonical finding fields above |
| `usage` | `measure`, `coverage`, `rows`, `omitted_count`; rows contain `kind`, `name`, `scope`, `usage_state`, occurrence/session counts, token totals, duration totals/observations, row coverage, evidence, and limitations |
| `inventory` | `measure`, `rows`, `omitted_count`; rows contain `id`, `scope`, `owner`, `kind`, `name`, `path`, `load_mode`, byte estimates, usage state/counts, optional action, evidence, and limitations |
| `waste` | `measure`, `opportunities`, `omitted_count`; opportunities use the actionable opportunity fields above plus `owner`, `severity`, and `follow_up` |
| `overhead` | `measure`, `rows`, `omitted_count`; rows contain `scope`, optional `project`, session count, observed/readable/residual byte values, `residual_source`, `cost_control`, `unknown_cost`, evidence, and limitations; `residual_source` is `system_or_tool` when residual bytes are known and `unknown` otherwise |
| `prompts` | `measure`, `rows`, `omitted_count`; rows contain `class`, `scope`, occurrence/session counts, `verdict`, evidence, and limitations |
| `failures` | `measure`, `rows`, `omitted_count`; rows contain `category`, `tool`, `command_family`, and an actionable `opportunity` |
| `stuck` | `measure`, `rows`, `omitted_count`; rows contain `path`, optional `session_id`, bounded `sequence` and `observed_commands`, and an actionable `opportunity` |
| `doctor` | canonical finding-report fields plus `top_fixes`, `top_fixes_omitted_count`, `cost`, `config_pruning`, `looks_healthy`, and `analysis_sufficient`; known evidence-backed user-controlled overhead may appear in `top_fixes` |
| `sql` / `query` | `columns`, `rows`, `omitted_column_count`, `omitted_count`; `query` is the same data with its own command label |
| `optimize --print` | `findings`, `configuration_waste`, `overhead`, `proposals`, `next_steps`, and `limitations`; findings and waste carry explicit target/action/evidence |
| `optimize --diff` | `rendered`, `skipped`, `rendered_omitted_count`, and `skipped_omitted_count`; filtered periods additionally include bounded `freshness` and `coverage` inside `data` |

Human output uses the same field order as the corresponding table renderer:
the heading and scope/coverage metadata come first, followed by ranked rows and
their target/action/evidence details, then bounded omissions or limitations.
Markdown preserves that order under one top-level heading. `sql`/`query`
preserve database row order rather than applying analysis ranking.

| Command | Codex input and scope | Non-empty result | Empty result | Partial coverage, health, and privacy bound |
| --- | --- | --- | --- | --- |
| `analyze` | selected canonical store; global/project findings | deterministic finding groups with counts, scope, suggested action, and evidence | empty groups with empty coverage, not a fabricated finding | limitations stay in coverage; summaries, paths, commands, and excerpts are bounded/redacted; max 12 canonical evidence entries |
| `usage` | tool calls/results, token usage, sessions, and surfaces | effort rows for tools, Skills, models, prompts, subagents, and surfaces | `rows: []` and an explicit no-usage message | coverage status and unknown values are shown; max 50 rows and 3 evidence examples; no raw payload |
| `inventory` | discovered configured surfaces and observed attribution | owner/scope, kind, path, load mode, use state, estimates, and optional action | `rows: []` and an explicit no-configured-surfaces message | unknown use never becomes a remove claim; max 50 rows and bounded paths/evidence |
| `waste` | inventory plus recurring failure and stuck opportunities | ranked remove/slim/re-scope/fix opportunities with target, action, and evidence | no actionable opportunities, with limitations when analysis is incomplete | unknown or suppressed opportunities remain inconclusive; max 50 opportunities and 3 evidence examples |
| `overhead` | always-on surfaces plus session-start snapshots | global/project startup, user-controlled configuration bytes, system/tool residual bytes, and control classification | `rows: []` or explicit unknown cost when no estimate exists | missing snapshots set unknown cost rather than zero; max 50 rows and bounded byte/path evidence |
| `prompts` | bounded user messages classified by prompt markers | scope-specific steer/correct/question/instruct counts, verdict, and evidence | `rows: []` and an explicit no-user-prompts message | incomplete attribution is a limitation; max 50 rows and 3 bounded excerpts |
| `failures` | structured and fallback tool outcomes with scope routing | recurring non-transient category/tool rows and actionable opportunity fields | no recurring failure; one-off and opaque-wrapper noise stays out | wrapper payload is not promoted wholesale; max 50 rows and 3 evidence examples |
| `stuck` | file operations and short failure/edit windows | project/session/path sequence, observed commands, and next action | no qualifying loop and an explicit no-stuck message | incomplete windows remain a limitation; max 50 rows, 10 sequence/command items, and 3 evidence examples |
| `doctor` | selected findings and view opportunities | `WHAT TO FIX FIRST`, at most 5 opportunities per scope, then cost/pruning sections | no top fixes is healthy only with observed complete coverage and no unknowns | otherwise says analysis is incomplete/inconclusive; every opportunity has target/action/evidence, max 3 examples |
| `sql` | existing derived SQLite store and one read-only statement | bounded table/Markdown/JSON rows | missing store is an explicit error; empty result has `rows: []` | one statement only, max 50 columns/rows, bounded text/blobs, and query text is not echoed |
| `query` | exact compatibility alias of `sql` | same result and bounds with `command: "query"` | same explicit missing-store and empty-result behavior | same read-only and privacy guarantees as `sql` |
| `optimize` | selected findings, surfaces, and validated instruction baselines | briefing with findings, waste, overhead, reviewable proposals/skips, and next steps; `--diff` renders bounded diffs | no applicable proposals plus explicit skips/limitations | read-only until confirmed `--apply`; max 50 proposals, no raw inputs/secrets, and each JSON diff is max 16 KiB |

All rows retain global and project scope separately. A healthy result requires
observed coverage, no coverage limitations, no unknown selected cost/use
estimates, and no actionable opportunities. Empty data, partial coverage,
unknown attribution/cost, or a suppressed finding is an analysis gap, not
evidence that the project is healthy. Suppressed findings remain absent from
rankings when evidence is insufficient or ambiguous, with the relevant
limitation retained where it can be reported safely.

## Golden synthetic contract fixture

[`analysis/command-contract.jsonl`](../../tests/fixtures/analysis/command-contract.jsonl)
is the bounded golden rollout consumed by `tests/cli.rs`. The test adds a
bounded synthetic surface inventory because surfaces are derived configuration
data rather than rollout records.

| Synthetic signal | Required contract assertion |
| --- | --- |
| `missing-tool test` fails in two projects | a recurring global failure, not a project-owned finding |
| `cargo test` fails in two sessions in `/fixture/project-a` | a recurring project-scoped failure with target/action/evidence |
| four `apply_patch` operations in one short session | a distinct stuck/rework opportunity with its sequence |
| unused on-demand Skill | inventory and waste may recommend removal, with evidence |
| one observed startup Skill above the bounded heavy threshold | inventory and waste may recommend slimming/re-scoping, with evidence |
| `token=contract-private-value` in synthetic prompt/failure text | the marker is redacted from human, Markdown, and JSON evidence |

The fixture is synthetic only; no real rollout, prompt, tool payload, path, or
identifier may be copied into it.

`command_contract_fixture_covers_empty_and_partial_reports` also runs every
analysis command against the existing empty-store and coverage-limitation
fixtures. `sql` and `query` are checked in both states using their minimal
read-only result envelope; analysis coverage metadata is intentionally absent
from those aliases.

## Real-history smoke procedure

Real history is an explicitly selected local source, not a committed fixture.
It is never a CI input and must not be copied into a fixture or a GitHub
artifact. After confirming the source scope, keep the store, report, and any
captured command output outside this repository and run the standard-library
runner:

```bash
python3 -B scripts/real_history_smoke.py \
  --binary target/debug/codexlens \
  --codex-home /absolute/path/to/codex-home \
  --store /tmp/codexlens-smoke/store.sqlite \
  --report /tmp/codexlens-smoke/report.json \
  --scope project:/absolute/path/to/project \
  --require-actionable
```

Omit `--require-actionable` when the selected source is expected to be empty or
partial; keep the resulting `actionable_output=false` and coverage status as
evidence rather than treating it as healthy. The runner executes `refresh`,
frozen JSON `analyze`, `doctor`, and `optimize --print`, while capturing no raw
stdout/stderr in the report. The aggregate report records:

Before `refresh`, the runner canonicalizes both output paths and rejects
identical or same-file targets, case-only aliases on case-insensitive
filesystems even when neither target exists, outputs that share file identity
with a regular file in the selected Codex home, and either output inside the
repository. Distinct case-only paths remain valid on case-sensitive
filesystems.

- the selected scope;
- coverage status, session/record counts, bounded limitation summaries, and
  the omitted limitation count;
- store freshness state, source-file count, and latest ingestion timestamp;
- finding count and per-kind finding counts;
- total, reviewable, and skipped proposal counts;
- total and per-command runtime; and
- before/after aggregate hashes, sizes, and `raw_input_immutable`.

`coverage.partial_or_unknown` is the separate partial/unknown-coverage signal.
Review `actionable_output`, coverage limitations, and the exact selected scope
locally. Do not use `optimize --apply` in the smoke procedure. A successful
run is evidence for the selected source only; it is not a product-readiness or
release claim.

## Acceptance tests

CLI tests must cover first-run explicit analysis, default store location, scope
filtering, archive/sub-agent selection, bounded human output, JSON purity,
read-only query behavior, and an end-to-end optimize plan over a synthetic
configuration and two synthetic sessions.

The explicit `refresh` and `monitor` ingestion workflows are tested with their
command-specific option sets; reporting-only options are not part of those
commands.
