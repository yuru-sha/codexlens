# codexlens

Analyze Codex sessions and turn recurring friction into actionable
`AGENTS.md` improvements.

> MVP status: local ingestion, instruction capture, deterministic lenses, the
> advisor, and the read-only reporting CLI are implemented. The binary reads a
> derived store; it does not ingest raw files or apply proposals.

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

The binary is a reporting surface over an existing derived SQLite store. It
does not create or refresh that store from raw rollout or state inputs. Every
command accepts `-s, --store PATH`, which defaults to `.codexlens.sqlite`.
Reports are human-readable and read-only with respect to the supplied store
and target instruction files. Legacy-store reporting may create a temporary
migrated copy, which is removed afterward; `optimize --diff` also reads the
recommended instruction files in order to render a diff.

| Command | Input | Output purpose | Read-only behavior |
| --- | --- | --- | --- |
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

`doctor` accepts the optional `--limit COUNT` to cap findings per scope.
`optimize` currently requires `--diff`; the command is advisory and
read-only. `analyze` reports every lens, while the focused analysis commands
report one lens through the same deterministic report format. Missing or
invalid stores return a bounded, actionable error. Older supported store
schemas are migrated only in a temporary copy, leaving the supplied store
unchanged.

The current binary has no ingestion or refresh command. Raw rollout/state
ingestion remains the adapter and store boundary, and reporting never reopens
those raw inputs.

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
```

The Phase 3 lenses and Phase 4 advisor remain exposed from the
`codexlens::analysis` and `codexlens::advisor` modules. The lenses consume
canonical data without reopening source files; the advisor reads only the
recommended instruction files when rendering diffs. See [the architecture specification](docs/specs/architecture.md),
[the session format contract](docs/specs/session-format.md), and
[the analysis contract](docs/specs/analysis.md).

## Deliberately deferred

- `optimize --apply`: requires an explicit write-safety contract, backups,
  patch validation, scope checks, and confirmation.
- Compressed rollout readers: plain JSONL is the current reader boundary;
  compressed inputs are reported as unsupported.
- `--frozen` reporting mode: skipping refresh is not a current CLI behavior;
  it will be specified together with any future refresh workflow.
- Machine-readable output and live monitoring: neither is part of the MVP
  command surface.

## Status and roadmap

Phases 0 through 4 are complete. The current MVP endpoint is the local,
deterministic reporting surface documented above; future work starts with the
deferred capabilities rather than an implicit expansion of the boundary.

- Phase 0 Foundation: [#1](https://github.com/yuru-sha/codexlens/issues/1)–[#4](https://github.com/yuru-sha/codexlens/issues/4)
- Phase 1 Codex ingestion: [#5](https://github.com/yuru-sha/codexlens/issues/5)–[#10](https://github.com/yuru-sha/codexlens/issues/10)
- Phase 2 Instructions: [#11](https://github.com/yuru-sha/codexlens/issues/11)–[#14](https://github.com/yuru-sha/codexlens/issues/14)
- Phase 3 Lenses: [#15](https://github.com/yuru-sha/codexlens/issues/15)–[#20](https://github.com/yuru-sha/codexlens/issues/20)
- Phase 4 Advisor: [#21](https://github.com/yuru-sha/codexlens/issues/21)–[#24](https://github.com/yuru-sha/codexlens/issues/24)

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
