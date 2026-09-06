# Final regression and safety audit

Status: passed for the Phase 6 release-preparation candidate.

Release candidate under audit: `a7ec8f2a11f8daa21074f686d80b176db92af4da`.
Observed 2026-09-07 (Asia/Tokyo).
The audit adds no product capability; it records the release evidence and keeps
the boundary checks discoverable.

## Quality gates

The local audit ran on macOS arm64 with Rust 1.92.0:

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | PASS |
| `cargo build --all-features` | PASS |
| `cargo test --all-features` | PASS — 168 tests, 5 suites |
| `cargo test --all-features --test cli` | PASS — 40 tests |

The release candidate's [GitHub Actions run](https://github.com/yuru-sha/codexlens/actions/runs/34049913765)
also passed all four checks: Ubuntu Rust 1.85.0 and 1.92.0, `macos-latest`, and
`windows-latest`. The macOS and Windows jobs run the pinned build/test
coverage.

## Determinism and privacy

- Human reports are covered by `reporting_commands_are_deterministic_and_aliases_match`;
  JSON reports and aliases are covered by
  `machine_readable_output_is_versioned_deterministic_and_canonical`.
- Human and JSON evidence is bounded and redacted by
  `reporting_is_deterministic_bounded_and_does_not_refresh_or_write`,
  `optimize_json_bounds_and_redacts_large_diff_content`, and
  `optimize_json_omits_redacted_diff_without_leaking_secret`.
- The complete reporting command surface is exercised by
  `reporting_command_surface_stays_read_only_and_private` for source read-only
  behavior and absence of a synthetic raw marker.
- The committed `tests/fixtures/` tree was scanned for real local paths,
  credentials, private keys, network-upload commands, and personal data.
  Both scans returned no matches; token-count field names are synthetic format
  data:

  ```text
  git grep -n -E '(/Users/|/home/|C:[\\/]|[A-Za-z]:[\\/]Users|BEGIN [A-Z ]+PRIVATE KEY|api[_-]?key|password|credential|authorization|bearer|sk-[A-Za-z0-9])' -- tests/fixtures
  git grep -n -E '(^|[[:space:][:punct:]])(curl|wget|ssh|git push|rm -rf|sudo|export [A-Z_]*(TOKEN|KEY|SECRET|PASSWORD)|OPENAI_API_KEY)' -- tests/fixtures
  ```

## Source read-only behavior

- Compressed ingestion preserves both compressed source files and isolates a
  corrupt sibling: `compressed_rollout_input_is_ingested_incrementally_and_read_only`
  and `corrupt_compressed_rollout_does_not_block_valid_sibling`.
- Refresh and frozen reporting preserve raw sources and the derived store:
  `refresh_and_frozen_reporting_are_explicit_and_read_only` and
  `failed_refresh_keeps_the_previous_derived_store`.
- Monitoring preserves its observed source and handles partial, rotated,
  truncated, duplicate, and restarted input in `tests/monitor.rs`.
- `optimize --diff` is read-only:
  `optimize_diff_renders_a_proposal_without_writing_the_target`.

## Apply recovery evidence

`optimize --apply` requires explicit confirmation and validates its write set,
scope, hashes, patch, and backups before writing. Success is covered by
`optimize_apply_requires_confirmation_and_applies_only_reviewed_proposals`.
Traversal, symlink, changed-hash, and tampered-patch rejection are covered by
`preflight_rejects_traversal_changed_hash_and_tampered_patch_without_writes`
and `symlink_components_are_rejected_before_writing`.
The multi-proposal failure path restores every changed file and retains
`manifest.tsv` plus each backup, covered by
`multi_proposal_failure_restores_all_files_and_keeps_backups`.

## Supported-platform boundary and findings

Local execution is macOS arm64. Rust 1.85.0 is verified through the Ubuntu,
macOS, and Windows hosted runners; Windows hosted-runner build/test coverage is
the supported verification boundary. No Windows runtime, packaging, or
installer support is claimed.

No release-blocking finding was observed. No speculative feature work or
follow-up issue was added by this audit.
