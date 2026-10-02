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

1. Read `docs/development/workflow.md` for implementation or remediation tasks;
   use its topic map to select the relevant specification under `docs/specs/`.
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
- Raw rollout/state inputs remain unchanged. Reports refresh the selected
  derived store unless `--frozen`; `refresh` updates it without reporting, and
  `monitor` may also write its selected cursor. `optimize --diff` produces
  review-only proposals; explicitly confirmed `optimize --apply` may update
  only its validated instruction/documentation write set.
- No network service or LLM is required by the MVP.

## Development

Run `sh scripts/verify.sh`, the shared local/CI gate. Setup and individual
commands are in `docs/development/environment.md`; CI also verifies Rust 1.85.0.

Prefer the standard library and existing dependencies. Do not add a
dependency, abstraction, or output format without a concrete issue or
measured need.

## Branch And Pull Request Workflow

- Do not edit, commit, or push directly to `main`. Make changes on a feature branch and merge them through a pull request.
- Direct work on `main` is allowed only when the user explicitly authorizes it.

## GitHub workflow

- GitHub Issues are the canonical work tracker.
- Shared Bug / Feature / Question forms and the default Pull Request template are inherited from `yuru-sha/.github`.
- Shared non-default labels, including `orca:*`, are synchronized from `yuru-sha/project-template`.
- Use `orca:*` labels only for ORCA execution state; do not treat them as release categories.
- `CHANGELOG.md` intentionally remains at repository root as a standard changelog convention.

## Commit Messages

- Follow the commit-message policy in `CONTRIBUTING.md`.

- Do not create commits unless the user explicitly requests it.

## Fixtures and privacy

- Tests use only synthetic data under `tests/fixtures/`.
- Tests for content that must not be committed to fixtures may construct a
  bounded synthetic value in memory; keep the surrounding fixture data under
  `tests/fixtures/` when practical.
- Never commit real rollout files, prompts, tool output, tokens, credentials,
  private repository paths, or personal identifiers.
- Issue and PR examples must be synthetic and bounded.
- Local SQLite stores are analysis artifacts, not repository fixtures.
