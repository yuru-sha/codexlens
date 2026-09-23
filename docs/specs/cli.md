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
codexlens optimize [options] [--print|--diff|--apply]
```

Analysis, view, doctor, and optimize commands refresh the selected derived
store before reporting unless `--frozen` is set. Refresh reads Codex inputs
without modifying them and incrementally replaces changed derived rows.
`--frozen` reads exactly the existing store without discovering raw inputs.
Use `analyze` to refresh and report all findings, or `refresh` to update the
store without rendering a report.
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

## Required and optional arguments

All report commands corresponding to cclens (`analyze`, the typed views,
`doctor`, and `optimize`) run without command-specific value arguments. Their
scope, format, store, source-home, time-window, and frozen options are optional
and have defaults. `sql` and `query` also accept no positional SQL and read it
from stdin. The CodexLens-only `monitor` command requires `--source PATH`; its
store and polling options are optional. `optimize --apply` retains the existing
explicit confirmation requirement (`--yes` for non-interactive use).

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

Bare `optimize` refreshes the store and launches an interactive `codex` session
with its bounded findings in a private temporary file. The prompt asks Codex
to investigate root causes and propose changes without editing files. `--frozen`
uses the existing store; `--print` prints the briefing instead of launching.
Proposal target files stay read-only until the user approves a concrete plan;
unless `--frozen` is set, `optimize` refreshes the derived store before reading
findings. It receives the same scoped findings as `doctor`, inspects the named configuration target,
and emits a plan containing exact target files, before/after snippets, reason,
evidence, and a verification step. Non-instruction targets without a retained
baseline carry bounded review-only metadata instead of before/after snippets.
No proposal-target write is allowed without explicit confirmation.
The `--print` briefing routes the same opportunities shown by `doctor` into its
prioritized findings, putting recurring work friction before configuration
trimming. Review-only opportunities name the target and sections to inspect,
state the root-cause question and possible change, and preserve unknowns without
inventing before/after text. Stdout stays bounded; detailed omitted findings,
proposals, and skips go to stderr in every format. JSON stdout includes the same
action semantics and explicit omitted/skipped counts.
An actionable non-shell failure is retained even when no canonical shell
command can be inferred; it is never turned into a shell prerequisite.

## cclens behavioral oracle

The user-facing meaning is derived from cclens, while Codex-specific parsing
remains behind codexlens' adapter. The comparison baseline is cclens commit
[`3df5f76`](https://github.com/lambdalisue/cclens/commit/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70)
(2026-09-24). The paired synthetic inputs live in
[analysis/cclens-command-contract](../../tests/fixtures/analysis/cclens-command-contract)
and [analysis/command-contract.jsonl](../../tests/fixtures/analysis/command-contract.jsonl).
[reference-output.json](../../tests/fixtures/analysis/cclens-command-contract/reference-output.json)
contains selected JSON content emitted by the pinned cclens executable. The
CLI test checks shared semantic signals against that output while allowing
product-specific envelopes, labels, and measurement values. These pinned
source locations are the behavioral oracle:

| Contract area | cclens source location at `3df5f76` |
| --- | --- |
| command enum, format flags, and dispatch/scope routing | [`src/cli.rs#L41-L227`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/cli.rs#L41-L227), [`src/cli.rs#L227-L318`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/cli.rs#L227-L318) |
| doctor ordering, empty state, cost, pruning, and healthy state | [`src/cli.rs#L756-L1037`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/cli.rs#L756-L1037) |
| stuck, failures, prompts, overhead, usage, and inventory views | [`src/cli.rs#L1178-L1236`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/cli.rs#L1178-L1236), [`src/cli.rs#L1247-L1388`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/cli.rs#L1247-L1388), [`src/cli.rs#L1390-L1585`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/cli.rs#L1390-L1585), [`src/cli.rs#L1919-L2262`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/cli.rs#L1919-L2262) |
| SQL bounds and read-only query behavior | [`src/cli.rs#L709-L755`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/cli.rs#L709-L755) |
| finding model, global/project routing, evidence, and action counts | [`src/core/optimize.rs#L17-L144`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/core/optimize.rs#L17-L144) |
| local-only optimization and briefing | [`src/core/optimize.rs#L219-L473`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/core/optimize.rs#L219-L473) |
| waste ranking and actionable opportunities | [`src/cli.rs#L2284-L2416`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/src/cli.rs#L2284-L2416) |
| doctor operational contract | [`doctor/SKILL.md`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/plugins/cclens/skills/doctor/SKILL.md) |
| optimize operational contract | [`optimize/SKILL.md`](https://github.com/lambdalisue/cclens/blob/3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70/plugins/cclens/skills/optimize/SKILL.md) |

### Codex input mapping

The following differences are intentional adapter boundaries, not changes to
the user-facing analysis meaning:

| cclens concept | CodexLens input/option | Preserved equivalence or explicit difference |
| --- | --- | --- |
| Claude transcript records and session database | rollout JSONL plus `state_*.sqlite` normalized into canonical records | tool outcomes, messages, sessions, token usage, file operations, and provenance feed the same lens categories; unknown valid records are retained, not guessed |
| Claude config and project instruction files | global/project `AGENTS.md`, `config.toml`, and discovered Codex surfaces | global/project ownership, bounded instruction evidence, and configuration actions remain separate |
| transcript input root (`--projects`) | `--codex-home` and input discovery | both select the local raw input root; Codex additionally discovers rollout JSONL and `state_*.sqlite` inputs under the selected home |
| scope filter (`--scope`) | `--scope global|project|project:PATH` | `project` means all known projects; `project:PATH` is normalized before matching; scope filters output, not ingestion |
| cclens database/report store | `--store PATH` derived SQLite store | reports refresh by default; `--frozen` reads the stored snapshot, while `refresh` ingests without a report |
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
| `optimize --print` | `findings`, `configuration_waste`, `overhead`, `proposals`, `next_steps`, and `limitations`; findings include a bounded canonical `key`, and findings and waste carry explicit target/action/evidence |
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

Both fixtures are synthetic only; no real rollout, prompt, tool payload, path,
or identifier may be copied into them. The paired-signal checks in
`command_contract_fixture_preserves_scopes_targets_and_evidence` compare
codexlens report content to the cclens executable snapshot, not just parser or
store behavior:

| Fixture signal | cclens observable meaning | codexlens assertion in `tests/cli.rs` |
| --- | --- | --- |
| three synthetic sessions | `analyze` reports the number of ingested sessions | session count equals the cclens JSON output |
| recurring failures in global and project scopes | `failures` groups recurring causes and routes each to an owner; `doctor` prioritizes actionable findings | `command_contract_fixture_preserves_scopes_targets_and_evidence`: failure categories, global/project routing, target/action/evidence, and doctor top fixes |
| repeated edits and failure/edit loop | `stuck` identifies a bounded re-edit episode; `waste` and `doctor` surface its action | same test: `src/lib.rs` sequence, stable `stuck:src/lib.rs|loop` identity, and target/action/evidence through waste/doctor/optimize |
| configured but unused Skill and observed heavy Skill | `inventory` reports usage/cost; `waste` ranks concrete cleanup | same test: inventory use state, remove/slim action, concrete target, and waste opportunity |
| user steering prompts | `prompts` classifies steer/correct/question/instruct behavior | same test: non-empty typed `prompts` rows with bounded evidence |
| multi-project session-start observations | `overhead` reports a global floor and per-project costs | same test: global row first and each project represented |
| identical read-only session-count query | `sql` returns bounded query results | cclens and codexlens `sql`/ `query` return the same synthetic session count |
| findings carried into the optimizer | `optimize --print` retains failures, stuck paths, and unused surfaces | same test: the corresponding codexlens `optimize --print` JSON findings and configuration waste retain those signals |
| private marker in synthetic input | cclens-style reports retain bounded evidence without exposing raw sensitive content | same test: table, Markdown, and JSON outputs omit the marker |

The cclens fixture README records the input structures and source revision
used to construct it. The snapshot omits volatile paths and freshness fields.
The test does not rebuild cclens; regenerating the reference requires building
the pinned source and running
`python3 scripts/refresh_cclens_contract_reference.py --source /path/to/cclens --cclens /path/to/cclens/target/debug/cclens`
against the checked-in synthetic input. The script rejects any source revision
other than the pinned SHA.

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
- before/after hashes and sizes for selected `state_*.sqlite` and rollout
  JSONL inputs, summarized by `raw_input_immutable`; and
- before/after whole-home hashes and sizes, summarized separately by
  `codex_home_unchanged`.

`coverage.partial_or_unknown` is the separate partial/unknown-coverage signal.
Review `actionable_output`, coverage limitations, and the exact selected scope
locally. `raw_input_immutable` compares only the history files selected for
ingestion: state databases and rollout JSONL under `sessions`, plus
`archived_sessions` when `--include-archived` is selected. The separate
`codex_home_unchanged` value covers all regular files in the selected Codex
home; unrelated concurrent writes may make it false without changing selected
history inputs. Neither comparison attributes a change to the smoke process.
The smoke fails on validation, command-execution, or report errors, or when
`--require-actionable` is set and no actionable output is produced. Do not use
`optimize --apply` in the smoke procedure. A successful run is one readiness
input for the selected source; it is not, by itself, a product readiness or
release approval.

## Acceptance tests

CLI tests must cover first-run explicit analysis, default store location, scope
filtering, archive/sub-agent selection, bounded human output, JSON purity,
read-only query behavior, and an end-to-end optimize plan over a synthetic
configuration and two synthetic sessions.

The explicit `refresh` and `monitor` ingestion workflows are tested with their
command-specific option sets; reporting-only options are not part of those
commands.
