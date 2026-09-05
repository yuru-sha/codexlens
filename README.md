# codexlens

Analyze Codex sessions and turn recurring friction into actionable
`AGENTS.md` improvements.

> Early development: the repository contains the foundation, ingestion,
> instruction capture, deterministic lenses, and the Phase 4 advisor.

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

The MVP does not send session data to a service, require an LLM, modify source
files, or claim billing accuracy.

## Current status

The reporting commands read an existing derived store (default:
`.codexlens.sqlite`) and never reopen raw rollout or state inputs:

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

`analyze` reports all findings; the other analysis commands report one lens.
`rework` also covers stuck activity and accepts the `stuck` alias, while
`knowledge` accepts `rediscovery`. All reports include store freshness and
bounded evidence. Missing or invalid stores return an actionable error.

The Phase 3 lenses and Phase 4 advisor are exposed from the
`codexlens::analysis` and `codexlens::advisor` modules. The lenses consume
canonical data without reopening source files; the advisor reads only the
recommended instruction files when rendering diffs. See [the architecture specification](docs/specs/architecture.md),
[the session format contract](docs/specs/session-format.md), and
[the analysis contract](docs/specs/analysis.md).

## Roadmap

- Phase 0 Foundation: [#1](issues/1)–[#4](issues/4)
- Phase 1 Codex ingestion: [#5](issues/5)–[#10](issues/10)
- Phase 2 Instructions: [#11](issues/11)–[#14](issues/14)
- Phase 3 Lenses: [#15](issues/15)–[#20](issues/20)
- Phase 4 Advisor: [#21](issues/21)–[#24](issues/24)

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
