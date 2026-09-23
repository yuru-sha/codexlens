# codexlens

[English](README.md) | [日本語](README.ja.md)

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/yuru-sha/codexlens)

Analyze Codex sessions and turn recurring friction into actionable
`AGENTS.md` improvements.

> Foundation MVP status: local ingestion, instruction capture, deterministic
> lenses, bounded reporting views, local monitoring, compressed rollout
> readers, versioned JSON output, and safe `optimize --apply` are implemented.
> Rebaseline [#143](https://github.com/yuru-sha/codexlens/issues/143) was closed
> by merged [#156](https://github.com/yuru-sha/codexlens/pull/156). The
> command contract and synthetic/CLI compatibility gates are implemented, but
> this is not a claim that product readiness is complete. An owner-authorized
> real-history smoke run against an explicitly selected local source remains
> the required readiness evidence.
> Reports refresh the derived store unless `--frozen`; explicit `refresh`
> ingests without reporting. Monitoring updates the derived store and an
> optional cursor file, and `optimize --apply` writes only its validated target
> set in addition to the normal store refresh.

## Goal

`codexlens` is a local, rule-based Codex harness optimizer:

The product contract is [`docs/specs/product.md`](docs/specs/product.md):
codexlens is intended to be the Codex counterpart of `cclens`, with a bounded
health check plus the corresponding inventory, overhead, usage, waste,
failures, stuck, prompts, sql/query, and optimize views.

```text
Codex local state + project instructions
        ↓
      adapter
        ↓
 canonical records → SQLite
        ↓
 failures / corrections / rework / verification /
 knowledge / instructions
        ↓
      findings
        ↓
 doctor / optimize
```

The MVP is designed to answer questions such as:

- Which failure or correction keeps recurring?
- Was the effective instruction chain present when it happened?
- Which verification steps are repeatedly missed?
- Which project knowledge is rediscovered across sessions?
- What small, scoped instruction change is supported by the evidence?

The MVP is local-only, deterministic, and evidence-backed. It does not send
session data to a service, require an LLM, modify source files outside the
validated instruction/documentation write set, or claim billing accuracy.

## CLI surface

The binary has an explicit refresh workflow and bounded reporting views over a
per-user derived SQLite store. Read views accept `--codex-home`, `-s,
--store`, `--scope global|project|project:PATH`, `--include-archived`,
`--include-subagents`, `--since`, `--until`, `--format table|markdown|json`,
and `--frozen`. The default store is
`${XDG_STATE_HOME:-~/.local/state}/codexlens/codexlens.db`; `analyze` and
`refresh` incrementally processes the selected Codex home; analysis, report,
doctor, and optimize commands refresh it automatically unless `--frozen` is
set. Bare `optimize` opens an interactive Codex investigation. Progress and
freshness diagnostics go to stderr, and
JSON stdout is one versioned document. Read-only reports also accept
reproducible period bounds; their output distinguishes the requested period,
observed coverage, and store freshness. Reporting updates only the derived
store and never the raw inputs. `optimize --apply` may also update its
validated instruction/documentation write set after confirmation.
Legacy-store reporting may create a temporary migrated copy, which is removed
afterward; `optimize --diff` also reads the recommended instruction files in
order to render a diff.

| Command | Input | Output purpose | Read-only behavior |
| --- | --- | --- | --- |
| `refresh` | discovered Codex home, rollout/state inputs, and instruction files | build or update the derived store | writes only the selected derived store; raw inputs remain unchanged |
| `analyze` | Codex home and derived store | all lens findings | refreshes the selected store unless `--frozen` |
| `sessions` | Codex home and derived store | bounded session metadata and coverage | refreshes unless `--frozen` |
| `inventory` | Codex home and derived store | configured surfaces, use, and startup estimates | refreshes unless `--frozen` |
| `overhead` | Codex home and derived store | always-on context cost and residual | refreshes unless `--frozen` |
| `usage` | Codex home and derived store | tools, Skills, models, prompts, subagents, and surface usage | refreshes unless `--frozen` |
| `waste` | Codex home and derived store | ranked remove/slim/re-scope opportunities | refreshes unless `--frozen` |
| `failures` | Codex home and derived store | recurring failures by normalized category and owner | refreshes unless `--frozen` |
| `corrections` | Codex home and derived store | correction-lens findings | refreshes unless `--frozen` |
| `rework` | Codex home and derived store | legacy rework findings | refreshes unless `--frozen` |
| `stuck` | Codex home and derived store | bounded edit/failure loops and affected paths | refreshes unless `--frozen` |
| `prompts` | Codex home and derived store | steer/correct/question/instruct patterns | refreshes unless `--frozen` |
| `verification` | Codex home and derived store | verification-lens findings | refreshes unless `--frozen` |
| `knowledge` | Codex home and derived store | knowledge-lens findings | refreshes unless `--frozen` |
| `rediscovery` | Codex home and derived store | alias for `knowledge` | refreshes unless `--frozen` |
| `instructions` | Codex home and derived store | instruction-lens findings | refreshes unless `--frozen` |
| `doctor` | Codex home and derived store | action-first health summary by scope | refreshes unless `--frozen` |
| `sql` | existing derived store and SQL/stdin | bounded read-only ad-hoc table, Markdown, or JSON rows | never refreshes or creates the store |
| `query` | existing derived store and SQL/stdin | bounded ad-hoc table, Markdown, or JSON rows | opens the store read-only; never refreshes or creates it |
| `optimize` | Codex home and derived store | interactive Codex root-cause investigation and proposed fixes | refreshes unless `--frozen`; asks Codex not to edit files |
| `optimize --diff` | Codex home, derived store, and target instruction files | high-confidence proposal diffs and skipped reasons | refreshes unless `--frozen`; does not modify target files |
| `optimize --apply --yes` | Codex home, derived store, and validated instruction/documentation targets | applies reviewed proposals and reports retained backups/recovery | refreshes unless `--frozen`; writes only the validated target set after confirmation |
| `monitor` | one local rollout JSONL or state SQLite source | bounded incremental ingestion and cursor/status output | does not modify the source; writes the derived store and optional cursor file |

`doctor` accepts the optional `--limit COUNT` to cap findings per scope.
`sql` accepts one positional SQL statement or reads SQL from stdin. It accepts
only a single read-only statement, limits output to 50 columns and 50 rows,
and never echoes the SQL in errors. JSON uses
`{columns, rows, omitted_column_count, omitted_count}` inside a versioned
`sql` envelope. `query` is retained as an explicit compatibility alias with
the same contract.

Bare `optimize` starts Codex with a private-file briefing; use `--print` to
inspect the briefing without launching Codex. `--diff` and `--print` are
advisory and read-only. `--apply` requires explicit
confirmation; non-interactive use must add `--yes` after reviewing the diff.
It validates the complete write set, re-reads and re-hashes every file, keeps
backups after success, and rolls back the whole batch on failure. `analyze`
reports every legacy lens, while the focused analysis commands report one
typed view through its own deterministic report format.
Add `--format json` to reporting commands for schema version 1;
aliases emit their canonical command name. Every view uses a versioned envelope
with top-level `scope`, `coverage`, and `freshness` metadata; its `data` object
contains named, bounded fields. Missing or invalid stores return a bounded,
actionable error. Older supported store schemas are migrated only in a
temporary copy. The selected store can still be updated by the normal refresh
unless `--frozen`.

Reporting periods use complete RFC3339 timestamps with `Z` or a numeric
`+HH:MM`/`-HH:MM` offset and optional fractional seconds (up to 9 digits).
Instants are compared in UTC and the interval is half-open: `[since, until)`.
Either bound may be omitted, equal bounds select no activity, and reversed or
invalid bounds fail without reading or changing the store. Relative periods
are intentionally not part of this interface. Period-filtered JSON adds
requested/observed bounds, included/excluded record counts, unknown
record/event counts, and `empty`/`complete`/`partial` state to the envelope
`coverage` object.
`optimize --apply` rejects period filters; `monitor` keeps its own ingestion
boundary.

`refresh` accepts `--codex-home PATH` (or `--home PATH`),
`--include-archived`, `--config PATH`, and `--store PATH`. Without
`--codex-home`, discovery uses `CODEX_HOME` and the platform default. A
successful refresh prints per-source ingest/skip summaries and recorded store
freshness; discovery and parse diagnostics remain bounded and explicit.

To build or update a store from raw inputs:

```bash
$ cargo run -- refresh --codex-home "$CODEX_HOME" --store .codexlens.sqlite
```

The adapter provides compressed rollout readers for plain and zstd-compressed rollout JSONL; reporting refreshes the derived store from raw inputs unless `--frozen` is set, then renders from the store. `monitor` is the explicit
local monitoring exception: it polls one rollout or state source, reuses the
existing adapter and canonical model, and appends or replaces only the derived
store. When requested, it also writes the bounded cursor to `--cursor PATH` at
a clean stop for reuse on the next invocation. Use `--kind rollout|state`;
`--max-polls COUNT` makes a finite run, and omitting it keeps polling at
`--interval-ms MILLISECONDS` until stopped.

The final MVP readiness review and next-phase entry condition are recorded in
[docs/readiness/mvp.md](docs/readiness/mvp.md). The Phase 6 regression and
safety evidence is recorded in
[docs/readiness/final-audit.md](docs/readiness/final-audit.md). Release notes,
the release checklist, and the minimal source-release procedure are in
[docs/release.md](docs/release.md); the current version history is in
[CHANGELOG.md](CHANGELOG.md).

The bounded, human-reviewed finding evaluation plan is in
[docs/evaluations/finding-usefulness-pilot.md](docs/evaluations/finding-usefulness-pilot.md).
It is a planning worksheet: real history requires explicit owner authorization,
and `optimize --apply` is outside the pilot.

The aggregate-only real-history smoke procedure is
[`scripts/real_history_smoke.py`](scripts/real_history_smoke.py), documented in
the [CLI specification](docs/specs/cli.md#real-history-smoke-procedure). Keep
the selected input, store, report, and any command output outside this
repository; the runner records only bounded counts, coverage and limitation
summaries, store freshness, timings, and raw-input immutability.

The command examples below use an existing derived store and opt into the
read-only `--frozen` boundary:

```bash
cargo run -- analyze --store .codexlens.sqlite --frozen
cargo run -- sessions --store .codexlens.sqlite --frozen
cargo run -- inventory --store .codexlens.sqlite --frozen
cargo run -- overhead --store .codexlens.sqlite --frozen
cargo run -- usage --store .codexlens.sqlite --frozen
cargo run -- waste --store .codexlens.sqlite --frozen
cargo run -- failures --store .codexlens.sqlite --frozen
cargo run -- stuck --store .codexlens.sqlite --frozen
cargo run -- prompts --store .codexlens.sqlite --frozen
cargo run -- corrections --store .codexlens.sqlite --frozen
cargo run -- rework --store .codexlens.sqlite --frozen
cargo run -- verification --store .codexlens.sqlite --frozen
cargo run -- knowledge --store .codexlens.sqlite --frozen
cargo run -- instructions --store .codexlens.sqlite --frozen
cargo run -- doctor --store .codexlens.sqlite --frozen --since 2026-01-01T00:00:00Z --until 2026-01-08T00:00:00Z
cargo run -- optimize --print --store .codexlens.sqlite --frozen
cargo run -- sql --store .codexlens.sqlite --format json SELECT/**/1
cargo run -- optimize --diff --store .codexlens.sqlite --frozen
cargo run -- monitor --source tests/fixtures/rollout/monitoring.jsonl --kind rollout --store .codexlens.sqlite --max-polls 1
cargo run -- doctor --format json --store .codexlens.sqlite --frozen
```

Reports update the selected derived store automatically. Run `analyze` for all
findings or `refresh` to ingest without a report. `--frozen` makes the store-only
boundary explicit: it does not discover raw inputs or write the store.
`Activity` is the earliest and latest valid
timestamp observed in the selected store; `Latest ingestion` is the separate
time the store recorded an input. Empty, missing, invalid, and partial
timestamp coverage is reported as such rather than filling activity dates
from ingestion time.

Example human-readable metadata prefix:

```text
Coverage: selected store (observed; not necessarily all historical activity or current raw inputs; refresh explicitly, archives via --include-archived)
Activity: 2026-01-03T00:00:00.000Z .. 2026-01-04T00:05:00.000Z
Activity timestamps: 16 valid, 0 missing, 0 invalid
Sessions: 2
Records: 16
Latest ingestion: 2026-09-09T00:00:00Z
```

The Phase 3 lenses, Phase 4 advisor, and Phase 5 safe-apply workflow remain exposed from the
`codexlens::analysis` and `codexlens::advisor` modules. The lenses consume
canonical data without reopening source files; the advisor reads only the
recommended instruction files when rendering diffs. See [the architecture specification](docs/specs/architecture.md),
[the session format contract](docs/specs/session-format.md), and
[the analysis contract](docs/specs/analysis.md). The compatibility contracts
for the implemented Phase 5 boundaries and the entry gate for future
extensions are defined in [docs/specs/post-mvp.md](docs/specs/post-mvp.md).

## Status and roadmap

Phases 0 through 5 implementation milestones are complete. Phase 5 compressed rollout readers (#57),
refresh/frozen reporting (#58), versioned JSON reporting (#59), bounded local
live monitoring (#60), and safe optimize apply (#61) are implemented. Future
changes must preserve the explicit boundaries documented above.

Rebaseline [#143](https://github.com/yuru-sha/codexlens/issues/143) was closed
by merged [#156](https://github.com/yuru-sha/codexlens/pull/156). The current
main hosted CI matrix passes. Product readiness still requires an
owner-authorized aggregate-only real-history smoke run against an explicitly
selected local source; CI and repository fixtures intentionally do not provide
that source.

Phase 6 is the release-preparation phase. Its release checklist and source
release procedure are recorded in [docs/release.md](docs/release.md).

- Phase 0 Foundation: [#1](https://github.com/yuru-sha/codexlens/issues/1)–[#4](https://github.com/yuru-sha/codexlens/issues/4)
- Phase 1 Codex ingestion: [#5](https://github.com/yuru-sha/codexlens/issues/5)–[#10](https://github.com/yuru-sha/codexlens/issues/10)
- Phase 2 Instructions: [#11](https://github.com/yuru-sha/codexlens/issues/11)–[#14](https://github.com/yuru-sha/codexlens/issues/14)
- Phase 3 Lenses: [#15](https://github.com/yuru-sha/codexlens/issues/15)–[#20](https://github.com/yuru-sha/codexlens/issues/20)
- Phase 4 Advisor: [#21](https://github.com/yuru-sha/codexlens/issues/21)–[#24](https://github.com/yuru-sha/codexlens/issues/24)
- Phase 5 compressed rollout reader milestone: [#57](https://github.com/yuru-sha/codexlens/issues/57) (implemented)
- Phase 5 refresh and frozen reporting: [#58](https://github.com/yuru-sha/codexlens/issues/58) (implemented)
- Phase 5 versioned JSON reporting: [#59](https://github.com/yuru-sha/codexlens/issues/59) (implemented)
- Phase 5 local live monitoring: [#60](https://github.com/yuru-sha/codexlens/issues/60) (implemented)
- Phase 5 safe optimize apply: [#61](https://github.com/yuru-sha/codexlens/issues/61) (implemented)

## Development

Requirements: Rust 1.85 or newer.

Local development is pinned by `rust-toolchain.toml`. Run the shared local/CI
gate with `sh scripts/verify.sh`; it also checks privacy and staged content.
See [environment setup](docs/agents/environment.md) and the
[delivery workflow](docs/agents/workflow.md) for permissions, review, and
completion criteria. The underlying Rust checks are:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
```

GitHub Actions runs the full gate with Rust 1.85.0 and 1.92.0 on Ubuntu.
The pinned `macos-14` arm64 and `windows-latest` jobs run the platform gate
(privacy checks, build, and all-feature tests); the macOS job also enforces the
reporting benchmark. These jobs use only the repository's synthetic fixtures
and do not require a real Codex home. Windows hosted-runner build/test coverage
is the verification boundary—Windows runtime behavior, packaging, and
installers are not supported claims.

See [AGENTS.md](AGENTS.md) for repository rules and the synthetic fixture
policy.

## Inspiration

The design was informed by:

- [`cclens`](https://github.com/lambdalisue/cclens) — the idea of joining
  configured surfaces with observed usage and separating adapters from a
  normalized store.
- [`codex-session-insights`](https://github.com/cosformula/codex-session-insights)
  — practical discovery of Codex local state and rollout files.

`codexlens` is an independent implementation. No code is copied from either
project.

## License

MIT. See [LICENSE](LICENSE).

## GitHub Release

See docs/agents/release.md for the release note format and creation procedure. The shared body template is .github/release-notes-template.md, and the generated-note categories are managed in .github/release.yml.
