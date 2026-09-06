# MVP readiness review

Status: ready to enter the next feature phase.

Review scope: the refactor and local reporting work through the current MVP
endpoint. The review checks the architecture, session-format, and analysis
specifications against the implementation, then verifies the supported CLI
surface and its boundaries.

## Verification

The reproducible local gates are:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --all-features --test cli
```

The CI workflow runs the same gates with Rust 1.85.0 and 1.92.0. The base
branch was green at review time (`main` at `2781536`); the current head must
pass the same matrix before this change is merged.

CLI integration coverage exercises every supported reporting command with
synthetic stores, including empty and minimal stores, aliases, deterministic
repeated runs, bounded errors, legacy-store migration, and the read-only
behavior of `optimize --diff`.

## Review result

- The CLI's user-facing reporting surface is limited to canonical-data lenses,
  derived-store reports, and review-only proposal diffs; ingestion and store
  APIs remain library boundaries rather than CLI reporting behavior.
- Finding and report ordering is deterministic; evidence retains source paths
  and line numbers where available, and reports bound/redact human-facing
  excerpts.
- Reporting reads the derived store without reopening raw rollout/state input.
  `optimize --diff` reads target instruction files but never writes them.
- No blocking issue remains for the current MVP reporting path. The
  architecture specification now records that corrections/findings are
  derived in memory; [#54](https://github.com/yuru-sha/codexlens/issues/54)
  closed the clarification. Any future persisted lens output needs its own
  explicit schema issue.

## Deferred work

The deferred capabilities and their rationale are tracked in
[#53](https://github.com/yuru-sha/codexlens/issues/53): compressed rollout
readers, refresh/frozen-mode behavior, machine-readable output, live
monitoring, and `optimize --apply`. They remain deferred because each expands
an input, runtime, output, or write boundary that needs its own contract.
The entry contracts and required compatibility/privacy test gates are in
[`docs/specs/post-mvp.md`](../specs/post-mvp.md).

The store-schema wording for `corrections` and `findings` was clarified and
closed in [#54](https://github.com/yuru-sha/codexlens/issues/54); the current
CLI derives those results in memory from canonical data.

## Next-phase entry condition

The next feature phase may start only when its issue:

1. selects a capability from #53, states its compatibility, privacy, and
   read-only/write requirements, and records explicit agreement on those
   acceptance criteria before implementation starts;
2. updates the relevant specification and adds synthetic regression coverage;
3. preserves the adapter → canonical data → derived store → lens/report
   boundary unless the issue explicitly changes that contract; and
4. passes the pinned CI matrix and documents any newly deferred work with an
   explicit rationale and tracking issue.
