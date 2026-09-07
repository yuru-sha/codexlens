# AGENTS.md

## Project

`codexlens` is a Rust CLI that reads local Codex history and turns repeated
friction into evidence-backed suggestions for `AGENTS.md` and related
project documentation.

- The project is MIT licensed.
- MVP processing is local-only, rule-based, and read-only with respect to
  Codex input files and source repositories.
- `cclens` and `codex-session-insights` are design and format references only.
  Do not copy their code.

## Artifact language

- Keep repository and GitHub artifacts in English by default: README and other
  docs, CHANGELOG, code comments, CLI messages, issues, pull requests, review
  comments, and releases.
- Keep the conversation with the user in the user's preferred language; this
  does not change the language of public project artifacts.
- Use another language in an artifact only when the user explicitly requests
  it or when quoting user-provided text. Before publishing, check that newly
  generated text matches the surrounding artifact language.

## Before changing code

1. Read the relevant specification under `docs/specs/`.
2. Keep upstream Codex format details inside an adapter; do not leak raw
   field names into analysis or storage code.
3. Add or update a deterministic synthetic fixture and a focused test for
   non-trivial behavior.
4. Keep the change within the issue scope. Update the specification when a
   decision changes.

## Architecture invariants

- Inputs: `state_*.sqlite`, rollout JSONL, `AGENTS.md` files, and
  `config.toml`.
- Flow: adapter → canonical records → SQLite store → lenses → findings →
  `doctor`/`optimize`.
- Unknown valid rollout records are retained with source provenance.
- Raw rollout/state inputs and the derived store are never edited. `optimize
  --diff` produces review-only proposals; explicit confirmed `optimize
  --apply` may update only its validated instruction/documentation write set.
- No network service or LLM is required by the MVP.

## Development

Use the pinned commands from CI:

`cargo fmt --all -- --check`
`cargo clippy --all-targets --all-features -- -D warnings`
`cargo test --all-features`

Prefer the standard library and existing dependencies. Do not add a
dependency, abstraction, or output format without a concrete issue or
measured need.

## Fixtures and privacy

- Tests use only synthetic data under `tests/fixtures/`.
- Tests for content that must not be committed to fixtures may construct a
  bounded synthetic value in memory; keep the surrounding fixture data under
  `tests/fixtures/` when practical.
- Never commit real rollout files, prompts, tool output, tokens, credentials,
  private repository paths, or personal identifiers.
- Issue and PR examples must be synthetic and bounded.
- Local SQLite stores are analysis artifacts, not repository fixtures.

## Agent skills

### Issue tracker

Issues and specs live in GitHub Issues, managed with the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Use the five canonical labels: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, and `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

This is a single-context repository. See `docs/agents/domain.md`.
