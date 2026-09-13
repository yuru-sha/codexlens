# CLI specification

This document is the executable command contract for the product described in
[`product.md`](product.md). The existing Rust module names are not part of the
user contract.

## Global options

Every command except `query` accepts:

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

## Pipeline

```text
codexlens analyze [options]
codexlens <view> [options]
codexlens query [SQL] [--store PATH] [--format table|markdown|json]
codexlens optimize [options] [--print]
```

`analyze` reads Codex inputs read-only and incrementally replaces changed
derived rows. Every view runs `analyze` first unless `--frozen` is present.
`query` is the only command that never creates or refreshes a store; it opens
an existing store read-only. Refresh chatter goes to stderr. JSON stdout is
one JSON document and contains no progress text.

## View contracts

| View | Required first section | Row identity | Required action/interpretation |
| --- | --- | --- | --- |
| `doctor` | `WHAT TO FIX FIRST` | opportunity id | problem, impact, target, fix, evidence |
| `inventory` | `CONFIGURATION INVENTORY` | scope + kind + path | usage state and startup/on-demand estimate |
| `overhead` | `CONTEXT COST` | scope + project | observed minimum session-start context and residual |
| `usage` | `WHERE EFFORT GOES` | surface/tool + period bucket | counts, tokens, duration, coverage |
| `waste` | `OPPORTUNITIES` | opportunity id | ranked concrete remove/slim/re-scope/fix action |
| `failures` | `RECURRING FAILURES` | category + tool | normalized cause, owner, examples, fix |
| `stuck` | `STUCK WORK` | project + session + path | edit burst/loop sequence and next investigation |
| `prompts` | `HOW YOU STEER CODEX` | prompt class | steer/correct/question/instruct counts and verdict |
| `sessions` | `SESSIONS` | session id | bounded metadata and selection coverage |

`doctor` renders at most 5 opportunities per scope and at most 3 evidence
examples per opportunity. Empty sections are omitted. `inventory`, `usage`,
and `sessions` render at most 50 rows by default and state the omitted count.
JSON contains the complete bounded result set selected by the same limits.

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
    "activity_end": null
  },
  "data": {}
}
```

View-specific `data` must use named fields, not rendered text. A finding row
must include `id`, `title`, `scope`, `target`, `impact`, `confidence`,
`occurrences`, `distinct_sessions`, `action`, `evidence`, and `limitations`.
Paths and excerpts are bounded and redacted before serialization.

## `optimize`

`optimize` is read-only until the user approves a concrete plan. It receives
the same scoped findings as `doctor`, inspects the named configuration target,
and emits a plan containing exact target files, before/after snippets, reason,
evidence, and a verification step. `--print` prints the briefing; otherwise
the implementation may launch the configured Codex CLI only through a private
temporary file. No mutating write is allowed without explicit confirmation.

## Acceptance tests

CLI tests must cover first-run auto-analysis, default store location, scope
filtering, archive/sub-agent selection, bounded human output, JSON purity,
read-only query behavior, and an end-to-end optimize plan over a synthetic
configuration and two synthetic sessions.
