# Repository Guidelines

## Project Overview

`codexlens` is a local-only Rust CLI that analyzes Codex session history and produces deterministic, evidence-backed suggestions for project instructions and documentation. It does not require a network service or LLM. Raw Codex inputs and source repositories remain read-only except for explicitly confirmed `optimize --apply` changes to validated instruction/documentation targets.

Public repository and GitHub artifacts are English by default. Do not copy code from cclens or codex-session-insights; they are design and format references only.

## Contribution Workflow

- Do not edit, commit, or push directly to `main`; use a feature branch and pull request. Direct work on `main` requires explicit user authorization.
- Follow `CONTRIBUTING.md`'s Conventional Commit format when a commit is authorized. Do not create commits unless explicitly requested.
- GitHub Issues are the canonical work tracker. Do not create tickets just to begin work.
- Never commit real rollout files, prompts, tool output, tokens, credentials, private repository paths, personal identifiers, or local analysis databases. Keep fixtures synthetic and bounded.

## Architecture & Data Flow

Keep upstream Codex formats inside the adapter boundary:

`discovery and adapters → canonical records → derived SQLite store → analysis lenses → findings and reports`

- Adapters discover and parse rollout JSONL and state SQLite, preserve unknown valid rollout records with source provenance, and normalize upstream details into `src/model.rs` types.
- `src/store.rs` persists canonical facts; lenses operate on stored canonical data and do not reopen raw inputs.
- Analysis is deterministic and evidence-backed. Keep evidence and report/query output bounded and redact sensitive content.
- `src/main.rs` coordinates CLI workflows and rendering; `src/advisor/` builds reviewable proposals and applies only the validated write set after explicit confirmation.
- Keep reporting refresh behavior, `--frozen`, monitor cursor writes, and apply boundaries consistent with the relevant specifications.

## Key Directories

- `src/`: CLI, adapters, canonical model, SQLite store, analysis, advisor, configuration, and monitoring.
- `src/analysis/`, `src/advisor/`: focused analysis lenses and proposal/report workflows.
- `tests/`: Rust integration tests; `tests/fixtures/` contains synthetic input data.
- `scripts/`: shared verification, privacy checks, and bounded smoke/pilot utilities.
- `docs/specs/`: product and implementation contracts; `docs/development/`: setup and delivery workflow.

## Development Commands

Rust is the primary runtime; Python 3.9+ standard library supports repository verification scripts. Install Rust through rustup and the platform C toolchain required by bundled SQLite/zstd.


```sh
cargo run -- --help
cargo build --all-features
cargo test --all-features
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
sh scripts/verify.sh
```

Use `sh scripts/verify.sh` as the shared local/CI gate. `sh scripts/verify.sh platform` omits formatting and Clippy but still runs privacy checks, build, and tests. The project targets Rust 1.85 MSRV and pins a local toolchain in `rust-toolchain.toml`; environment/setup details are in `docs/development/environment.md`.


## Code Conventions & Common Patterns

- Follow the adapter → canonical model → store → analysis layering; do not expose raw upstream field names to lenses or storage code.
- Keep analysis and outputs deterministic; never rely on SQLite row order or hash-map iteration for report order.
- Bound input line sizes, retained text, evidence, and query/report output. Represent recoverable malformed or incomplete input as diagnostics where the module already provides that pattern.
- Use domain types and diagnostics in lower-level modules; use `anyhow::Result` at orchestration boundaries. Propagate errors with `?`; reject invalid CLI combinations explicitly.
- Prefer existing dependencies and standard-library solutions. Keep changes focused and preserve read-only input boundaries.
- Public repository artifacts and CLI messages are English by default.

## Code, Tests, Commits, and Comments

- Code should make how the behavior is implemented clear.
- Tests should make what behavior is expected clear.
- Commit messages should explain why the change is needed.
- Comments should explain why not an apparently reasonable alternative was chosen, including relevant constraints or non-obvious intent. Do not use comments to paraphrase the code's behavior.

## Important Files

- `src/main.rs`: CLI definitions, dispatch, and user-facing workflow orchestration.
- `src/lib.rs`: core module exports.
- `src/model.rs`, `src/normalize.rs`, `src/rollout.rs`, `src/state.rs`: canonical records and input adaptation.
- `src/store.rs`, `src/analysis.rs`, `src/advisor/`: persistence, findings, and optimization proposals.
- `Cargo.toml`, `rust-toolchain.toml`: package, dependencies, and toolchain.
- `docs/development/workflow.md`: contract-to-code map and implementation/verification procedure.
- `scripts/verify.sh`: shared verification gate.

## Runtime/Tooling Preferences

Use Cargo and the repository-pinned Rust toolchain; there is no JavaScript runtime or package manager requirement. Rust commands in the verification gate use `--locked` (formatting is the exception). The verification scripts use `python3` by default and honor `PYTHON`. Do not add dependencies, abstractions, or output formats without a concrete need.

## Testing & QA

- Rust tests use Cargo’s built-in test framework: unit tests live alongside modules; CLI and monitor integration tests are in `tests/cli.rs` and `tests/monitor.rs`.
- Add focused tests and deterministic synthetic fixtures for non-trivial behavior. Fixtures must be fictional; never copy or sanitize real rollout files into the repository. Use temporary directories/stores and bounded in-memory synthetic content where appropriate.
- Python privacy and smoke-runner tests are included in `scripts/verify.sh`.
- Run a focused test for the changed path, then `sh scripts/verify.sh`. Check the relevant contract under `docs/specs/` and keep that specification current when a decision changes.
- Read `docs/development/workflow.md` first for implementation/remediation tasks; its topic map identifies the relevant specification, implementation files, and checks.
