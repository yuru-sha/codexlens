# codexlens

Analyze Codex sessions and turn recurring friction into actionable
`AGENTS.md` improvements.

> Early development: the repository contains the foundation, Phase 1 ingestion
> library, and Phase 2 instruction resolver, snapshots, and joins. Lenses and
> reporting remain tracked in the follow-up issues.

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

The initial CLI exposes the planned command names so the public interface can
be discussed before implementation:

```bash
cargo run -- --help
```

See [the architecture specification](docs/specs/architecture.md),
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
