# MVP readiness review

Status: Rebaseline #143 was closed by merged #156; its implementation and
synthetic/CLI gates are complete, and current main hosted CI passes. The
owner-authorized real-history smoke recorded below did not pass, so this
document is not a current release-readiness approval.

Review scope: the refactor, Phase 5 input/reporting work, and safe
proposal-apply workflow through the historical MVP endpoint. The review checks the
architecture, session-format, analysis, and post-MVP write contract against the
implementation, then verifies the supported CLI surface and its boundaries.

## Verification

The reproducible local gates are:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --all-features --test cli
```

The Ubuntu CI workflow runs the same gates with Rust 1.85.0 and 1.92.0. The
macOS and Windows hosted-runner jobs run `cargo build --all-features` and
`cargo test --all-features` with Rust 1.85.0. These jobs use only synthetic
repository fixtures and do not require a real Codex home. Windows hosted-runner
build/test coverage is the verification boundary; no Windows runtime,
packaging, or installer support is claimed.

The final Phase 6 regression and safety evidence is recorded in
[final-audit.md](final-audit.md).

CLI integration coverage exercises every supported reporting command with
synthetic stores, including empty and minimal stores, aliases, deterministic
repeated runs, bounded errors, legacy-store migration, read-only `--diff`, and
confirmed safe apply behavior. The Phase 5 coverage also exercises compressed
rollout input, explicit refresh and frozen reporting, versioned JSON output, and
bounded local monitoring.

## Review result

- The CLI's user-facing surface includes explicit refresh, bounded local
  monitoring, canonical-data lenses, derived-store reports, review-only
  proposal diffs, and the explicitly confirmed validated apply workflow.
  Reporting never refreshes implicitly, and raw input sources remain read-only.
- Finding and report ordering is deterministic; evidence retains source paths
  and line numbers where available, and reports bound/redact human-facing
  excerpts.
- Reporting and apply read the derived store without reopening raw rollout/state
  input. `optimize --diff` never writes target instruction files, while
  `optimize --apply` writes only its validated write set and retains backups.
- No blocking issue remains for the historical MVP reporting path. The
  architecture specification now records that corrections/findings are
  derived in memory; [#54](https://github.com/yuru-sha/codexlens/issues/54)
  closed the clarification. Any future persisted lens output needs its own
  explicit schema issue.

## Implemented Phase 5 boundaries

Phase 5 is complete. The capabilities and their rationale were originally
tracked in [#53](https://github.com/yuru-sha/codexlens/issues/53). Issues
[#57](https://github.com/yuru-sha/codexlens/issues/57),
[#58](https://github.com/yuru-sha/codexlens/issues/58),
[#59](https://github.com/yuru-sha/codexlens/issues/59),
[#60](https://github.com/yuru-sha/codexlens/issues/60), and
[#61](https://github.com/yuru-sha/codexlens/issues/61) implement the
compressed rollout reader, refresh/frozen reporting, machine-readable output,
live monitoring, and safe apply under their agreed contracts.
The entry contracts and required compatibility/privacy test gates are in
[`docs/specs/post-mvp.md`](../specs/post-mvp.md).

The store-schema wording for `corrections` and `findings` was clarified and
closed in [#54](https://github.com/yuru-sha/codexlens/issues/54); the current
CLI derives those results in memory from canonical data.

## Product rebaseline and remaining readiness gate

The product rebaseline in [#143](https://github.com/yuru-sha/codexlens/issues/143)
was closed by merged [#156](https://github.com/yuru-sha/codexlens/pull/156).
That delivery completed the documented CLI help, required views, optimize
briefing, bounded synthetic end-to-end evidence, and aggregate-only
real-history smoke procedure; current main hosted CI passes. The remaining
readiness evidence is a successful owner-authorized real-history smoke run.
The Issue #166 attempt below does not satisfy that gate.

## Issue #166 real-history smoke outcome

- Latest authorized attempt: exit 1; `raw_input_immutable=false`.
- Before/after snapshot: +2 files, +32,768 bytes; the runner does not attribute
  the mutation.
- Coverage: `empty`; `partial_or_unknown=false` (0 sessions, 0 records, 0
  limitations).
- Findings: 0. Proposals: 50 total, 0 reviewable, 50 skipped.
- `actionable_output=false`.

This run is not readiness evidence. The readiness gate remains open until a run
records `raw_input_immutable=true`.

## Phase 6 entry condition

Phase 6 release-preparation work may proceed only when its issue:

1. states the affected CLI boundary, compatibility, privacy, and
   read-only/write requirements, and records explicit agreement on those
   acceptance criteria before implementation starts;
2. updates the relevant specification and adds synthetic regression coverage;
3. preserves the adapter → canonical data → derived store → lens/report
   boundary unless the issue explicitly changes that contract; and
4. passes the pinned CI matrix and documents any newly deferred work with an
   explicit rationale and tracking issue.
