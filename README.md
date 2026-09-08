# codexlens

Analyze Codex sessions and turn recurring friction into actionable
`AGENTS.md` improvements.

> MVP status: local ingestion, instruction capture, deterministic lenses, the
> advisor, the read-only reporting CLI, bounded local monitoring, compressed
> rollout readers, explicit refresh/frozen reporting, versioned JSON output,
> and safe `optimize --apply` are implemented. `refresh` is the explicit raw-input workflow;
> reporting never refreshes implicitly, monitoring updates the derived store
> and an optional cursor file, and apply writes only its validated write set.

## Goal

`codexlens` is a local, rule-based Codex harness optimizer:

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

The binary has an explicit refresh workflow and a separate reporting surface
over the derived SQLite store. Reporting commands accept `-s, --store PATH`,
which defaults to `.codexlens.sqlite`, `--frozen` to state that the selected
store must be used exactly as recorded, and `--format json` for the versioned
machine-readable schema. Both normal and frozen reporting formats are
read-only with respect to raw inputs and never refresh implicitly. The explicit
`optimize --apply` path is the write exception and may update only its
validated instruction/documentation write set.
Legacy-store reporting may create a temporary migrated copy, which is removed
afterward; `optimize --diff` also reads the recommended instruction files in
order to render a diff.

| Command | Input | Output purpose | Read-only behavior |
| --- | --- | --- | --- |
| `refresh` | discovered Codex home, rollout/state inputs, and instruction files | build or update the derived store | writes only the selected derived store; raw inputs remain unchanged |
| `analyze` | derived store | all lens findings | reads the store only |
| `sessions` | derived store | stored session metadata, coverage, and freshness | reads the store only |
| `failures` | derived store | failure-lens findings | reads the store only |
| `corrections` | derived store | correction-lens findings | reads the store only |
| `rework` | derived store | rework and stuck findings | reads the store only |
| `stuck` | derived store | alias for `rework` | reads the store only |
| `verification` | derived store | verification-lens findings | reads the store only |
| `knowledge` | derived store | knowledge-lens findings | reads the store only |
| `rediscovery` | derived store | alias for `knowledge` | reads the store only |
| `instructions` | derived store | instruction-lens findings | reads the store only |
| `doctor` | derived store | coverage and ranked findings grouped by scope | reads the store only |
| `optimize --diff` | derived store and target instruction files | high-confidence proposal diffs and skipped reasons | does not modify the supplied store or target files; legacy stores use a temporary migrated copy |
| `optimize --apply --yes` | derived store and validated instruction/documentation targets | applies reviewed proposals and reports retained backups/recovery | modifies only the validated write set; never modifies the supplied store or rollout/state inputs |
| `monitor` | one local rollout JSONL or state SQLite source | bounded incremental ingestion and cursor/status output | does not modify the source; writes the derived store and optional cursor file |

`doctor` accepts the optional `--limit COUNT` to cap findings per scope.
`optimize` requires exactly one of `--diff` or `--apply`. `--diff` is advisory
and read-only. `--apply` requires explicit confirmation; non-interactive use
must add `--yes` after reviewing the diff. It validates the complete write set,
re-reads and re-hashes every file, keeps backups after success, and rolls back
the whole batch on failure. `analyze` reports every lens, while the focused
analysis commands report one lens through the same deterministic report format.
Add `--format json` to read-only reporting commands for schema version 1;
aliases emit their canonical command name. Sessions and finding reports include
an additive `coverage` object with the selected-store scope, valid activity
range, session/record counts, and missing/invalid timestamp counts. Existing
schema fields and aliases remain unchanged. Missing or invalid stores return a
bounded, actionable error. Older supported store schemas are migrated only in
a temporary copy, leaving the supplied store unchanged.

`refresh` accepts `--codex-home PATH` (or `--home PATH`),
`--include-archived`, `--config PATH`, and `--store PATH`. Without
`--codex-home`, discovery uses `CODEX_HOME` and the platform default. A
successful refresh prints per-source ingest/skip summaries and recorded store
freshness; discovery and parse diagnostics remain bounded and explicit.

To build or update a store from raw inputs:

```bash
$ cargo run -- refresh --codex-home "$CODEX_HOME" --store .codexlens.sqlite
```

The adapter provides compressed rollout readers for plain and zstd-compressed rollout JSONL; reporting never reopens raw inputs. `monitor` is the explicit
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

The command examples below use an existing derived store at the default path:

```bash
cargo run -- analyze --store .codexlens.sqlite
cargo run -- sessions --store .codexlens.sqlite
cargo run -- failures --store .codexlens.sqlite
cargo run -- corrections --store .codexlens.sqlite
cargo run -- rework --store .codexlens.sqlite
cargo run -- verification --store .codexlens.sqlite
cargo run -- knowledge --store .codexlens.sqlite
cargo run -- instructions --store .codexlens.sqlite
cargo run -- doctor --store .codexlens.sqlite
cargo run -- optimize --diff --store .codexlens.sqlite
cargo run -- monitor --source tests/fixtures/rollout/monitoring.jsonl --kind rollout --store .codexlens.sqlite --max-polls 1
cargo run -- doctor --format json --store .codexlens.sqlite
```

Read-only reports make the data boundary explicit. `Activity` is the earliest
and latest valid timestamp observed in the selected store; `Latest ingestion`
is the separate time the store recorded an input. An unfiltered report is not
necessarily all historical activity or the current raw inputs. Run `refresh`
explicitly to update the store, and pass `--include-archived` during refresh to
opt in to archived sessions; reporting never refreshes implicitly. Empty,
missing, invalid, and partial timestamp coverage is reported as such rather
than filling activity dates from ingestion time.

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

Phases 0 through 5 are complete. Phase 5 compressed rollout readers (#57),
refresh/frozen reporting (#58), versioned JSON reporting (#59), bounded local
live monitoring (#60), and safe optimize apply (#61) are implemented. Future
changes must preserve the explicit boundaries documented above.

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
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

GitHub Actions runs the build and full test suite with Rust 1.85.0 on
`macos-latest` and `windows-latest`; these tests use only the repository's
synthetic fixtures and do not require a real Codex home. Windows hosted-runner
build/test coverage is the verification boundary—Windows runtime behavior,
packaging, and installers are not supported claims.

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
