# MVP readiness review

Status: Rebaseline #143 was closed by merged #156; its implementation and
synthetic/CLI gates are complete, and current main hosted CI passes. The
owner-authorized real-history smoke below satisfies Issue #166's acceptance
criteria; this review records evidence and is not a release-readiness approval.

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

## Product rebaseline and Issue #166 smoke evidence

The product rebaseline in [#143](https://github.com/yuru-sha/codexlens/issues/143)
was closed by merged [#156](https://github.com/yuru-sha/codexlens/pull/156).
That delivery completed the documented CLI help, required views, optimize
briefing, bounded synthetic end-to-end evidence, and aggregate-only
real-history smoke procedure; current main hosted CI passes. Issue #166 requires
a completed owner-authorized real-history smoke run with a sanitized record of
command outcome, coverage, actionability, and stability diagnostics. Partial coverage
and `raw_input_immutable=false` are reported outcomes, not additional failure
conditions. The run below satisfies Issue #166's acceptance criteria; this
record does not itself approve a release.

## Issue #166 real-history smoke outcome

- Latest completed owner-authorized run: exit 0; `raw_input_immutable=false`.
- Before/after snapshot: +4 files and +5,037,632 bytes; the runner does not
  attribute the mutation.
- Coverage: partial; `partial_or_unknown=true`; 120 sessions, 166,336 records,
  12 limitations.
- Findings: 22. Proposals: 50 total, 0 reviewable, 50 skipped;
  `actionable_output=true`.

This completed run satisfies Issue #166's acceptance criteria: command
execution succeeded, the aggregate result was recorded, and actionable output
was present when required. Partial coverage and zero reviewable proposals are
reported results, not additional acceptance failures. The older smoke schema
used `raw_input_immutable` for its whole-home snapshot; those historical values
do not attribute changes to CodexLens. The current schema separately compares
selected history inputs and the whole-home snapshot.

## Issue #167 current-main smoke recheck

- Binary commit: `78a81856b6694075515101fecb97c18bc0fe0f61`; run date:
  2026-09-22; source: the owner-selected default Codex home; scope: `all`.
- Result: exit 0 in 963.902 seconds; 439 sessions, 366,773 records, partial
  coverage, 12 limitations, and 33,538 limitations omitted from the bounded
  report.
- Findings: 112 total (51 gap, 16 rework, 10 stale, 1 stuck, 34 verification).
  Proposals: 50 total, 0 reviewable, 50 skipped; `actionable_output=true`.
- Legacy whole-home snapshot: changed by +2 files and +9,885,862 bytes. This
  earlier run predates the selected-history comparison and does not identify
  the writer or prove the parent Issue's raw-input-unchanged criterion.

## Issue #167 selected-history recheck (PR head)

- Binary commit: `8342337ef6342072c94c7c40583d9dbc24975f1d`; run date:
  2026-09-22; source: the owner-selected default Codex home; scope: `all`.
- Result: exit 0; 440 sessions, 369,687 records, partial coverage, 12
  limitations, and 33,716 limitations omitted from the bounded report.
- Findings: 112 total (51 gap, 16 rework, 10 stale, 1 stuck, 34 verification).
  Proposals: 50 total, 0 reviewable, 50 skipped; `actionable_output=true`.
- Selected-history snapshot: `raw_input_immutable=false`; +2 files and
  +3,714,178 bytes. The whole-home snapshot was also changed (+0 files,
  +4,836,109 bytes). The smoke does not identify the writer, so the parent
  Issue's raw-input-unchanged criterion remains unmet.

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
