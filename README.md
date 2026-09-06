# codexlens

Analyze Codex sessions and turn recurring friction into actionable
`AGENTS.md` improvements.

> MVP status: local ingestion, instruction capture, deterministic lenses, the
> advisor, the read-only reporting CLI, bounded local monitoring, compressed
> rollout readers, explicit refresh/frozen reporting, and versioned JSON output
> are implemented. `refresh` is the explicit raw-input workflow; reporting
> never refreshes implicitly, and monitoring updates only the derived store.

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
session data to a service, require an LLM, modify source files, or claim
billing accuracy.

## CLI surface

The binary has an explicit refresh workflow and a separate reporting surface
over the derived SQLite store. Reporting commands accept `-s, --store PATH`,
which defaults to `.codexlens.sqlite`, `--frozen` to state that the selected
store must be used exactly as recorded, and `--format json` for the versioned
machine-readable schema. Both normal and frozen reporting formats are
read-only with respect to raw inputs and never refresh implicitly.
Legacy-store reporting may create a temporary migrated copy, which is removed
afterward; `optimize --diff` also reads the recommended instruction files in
order to render a diff.

| Command | Input | Output purpose | Read-only behavior |
| --- | --- | --- | --- |
| `refresh` | discovered Codex home, rollout/state inputs, and instruction files | build or update the derived store | writes only the selected derived store; raw inputs remain unchanged |
| `analyze` | derived store | all lens findings | reads the store only |
| `sessions` | derived store | stored session metadata and freshness | reads the store only |
| `failures` | derived store | failure-lens findings | reads the store only |
| `corrections` | derived store | correction-lens findings | reads the store only |
| `rework` | derived store | rework and stuck findings | reads the store only |
| `stuck` | derived store | alias for `rework` | reads the store only |
| `verification` | derived store | verification-lens findings | reads the store only |
| `knowledge` | derived store | knowledge-lens findings | reads the store only |
| `rediscovery` | derived store | alias for `knowledge` | reads the store only |
| `instructions` | derived store | instruction-lens findings | reads the store only |
| `doctor` | derived store | ranked findings grouped by scope | reads the store only |
| `optimize --diff` | derived store and target instruction files | high-confidence proposal diffs and skipped reasons | does not modify the supplied store or target files; legacy stores use a temporary migrated copy |
| `monitor` | one local rollout JSONL or state SQLite source | bounded incremental ingestion and cursor/status output | reads the source only; writes only the derived store |

`doctor` accepts the optional `--limit COUNT` to cap findings per scope.
`optimize` currently requires `--diff`; the command is advisory and
read-only. `analyze` reports every lens, while the focused analysis commands
report one lens through the same deterministic report format. Add
`--format json` to any reporting command for schema version 1; aliases emit
their canonical command name. Missing or invalid stores return a bounded,
actionable error. Older supported store schemas are migrated only in a
temporary copy, leaving the supplied store unchanged.

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
store. Use `--kind rollout|state`; `--max-polls COUNT` makes a finite run, and
omitting it keeps polling at `--interval-ms MILLISECONDS` until stopped.

The final MVP readiness review, verification evidence, and next-phase entry
condition are recorded in [docs/readiness/mvp.md](docs/readiness/mvp.md).

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

The Phase 3 lenses and Phase 4 advisor remain exposed from the
`codexlens::analysis` and `codexlens::advisor` modules. The lenses consume
canonical data without reopening source files; the advisor reads only the
recommended instruction files when rendering diffs. See [the architecture specification](docs/specs/architecture.md),
[the session format contract](docs/specs/session-format.md), and
[the analysis contract](docs/specs/analysis.md). The deferred next-phase
contracts are defined in [docs/specs/post-mvp.md](docs/specs/post-mvp.md).

## Deliberately deferred

- `optimize --apply`: requires an explicit write-safety contract, backups,
  patch validation, scope checks, and confirmation. Tracked in [#53](https://github.com/yuru-sha/codexlens/issues/53).
## Status and roadmap

Phases 0 through 4 are complete. Phase 5 compressed rollout readers (#57),
refresh/frozen reporting (#58), versioned JSON reporting (#59), and bounded
local live monitoring (#60) are implemented; the remaining deferred
capabilities are documented above.

- Phase 0 Foundation: [#1](https://github.com/yuru-sha/codexlens/issues/1)–[#4](https://github.com/yuru-sha/codexlens/issues/4)
- Phase 1 Codex ingestion: [#5](https://github.com/yuru-sha/codexlens/issues/5)–[#10](https://github.com/yuru-sha/codexlens/issues/10)
- Phase 2 Instructions: [#11](https://github.com/yuru-sha/codexlens/issues/11)–[#14](https://github.com/yuru-sha/codexlens/issues/14)
- Phase 3 Lenses: [#15](https://github.com/yuru-sha/codexlens/issues/15)–[#20](https://github.com/yuru-sha/codexlens/issues/20)
- Phase 4 Advisor: [#21](https://github.com/yuru-sha/codexlens/issues/21)–[#24](https://github.com/yuru-sha/codexlens/issues/24)
- Phase 5 compressed rollout reader milestone: [#57](https://github.com/yuru-sha/codexlens/issues/57) (implemented)
- Phase 5 refresh and frozen reporting: [#58](https://github.com/yuru-sha/codexlens/issues/58) (implemented)
- Phase 5 versioned JSON reporting: [#59](https://github.com/yuru-sha/codexlens/issues/59) (implemented)
- Phase 5 local live monitoring: [#60](https://github.com/yuru-sha/codexlens/issues/60) (implemented)

## Development

Requirements: Rust 1.85 or newer.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

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
