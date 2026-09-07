# Delivery workflow

## Scope and sources

Record the requested outcome, acceptance criteria, current HEAD, and starting
worktree status. Preserve unrelated changes. A diagnosis or review is read-only;
an implementation request authorizes scoped edits and their verification.

| Topic | Contract | Implementation / checks |
| --- | --- | --- |
| Boundaries and canonical model | `docs/specs/architecture.md` | `src/model.rs`, `src/store.rs` |
| Input formats and discovery | `docs/specs/session-format.md` | `src/discovery.rs`, `src/rollout.rs`, `src/normalize.rs`, `src/state.rs` |
| Findings and advice | `docs/specs/analysis.md` | `src/analysis/`, `src/advisor/` |
| Refresh, frozen/JSON reports, monitor, apply | `docs/specs/post-mvp.md` | `src/main.rs`, `src/monitor.rs`, `src/advisor/apply.rs`, `tests/cli.rs`, `tests/monitor.rs` |
| Setup and permissions | [environment.md](environment.md) | `.codex/config.toml`, `scripts/verify.sh`, CI |
| Releases | [../release.md](../release.md) | `docs/readiness/` records historical candidate evidence |

Fetch the relevant issue when supplied and access is authorized. Use the user's
request as acceptance criteria when there is no issue; do not create a ticket
just to start. Read only the contracts and callers relevant to the change.

## Implement and verify

1. For a behavior fix, trace all callers and reproduce the root cause with a
   focused synthetic check. Use the available `tdd` skill for requested TDD;
   otherwise keep the regression check proportional to the behavior.
2. Implement the smallest scoped change. Repository fixture requirements take
   precedence over a generic simplification skill's advice to omit fixtures.
3. Run the focused check, then `sh scripts/verify.sh`. Fix attributable failures
   and rerun affected gates. Report pre-existing failures separately. Do not
   convert an unavailable or failed check into a pass.
4. Review Standards (documented repository rules) and Spec (each acceptance
   criterion and boundary) independently. Use `code-review` when available and
   a nonempty committed comparison exists. For uncommitted work, include tracked
   diffs and all intended new files; do not commit just to enable that skill.
5. Fix confirmed findings, rerun affected checks, and repeat the affected review
   until no actionable in-scope finding remains. If progress needs a missing
   decision, permission, or external repair, record the exact blocker and the
   safe work already completed; do not repeat unchanged failing attempts.

For non-trivial changes, parallel read-only Standards and Spec subagents are
authorized when available. Give both the same base/HEAD, acceptance criteria,
and intended file list. Freeze edits while they review uncommitted files and
include the same diff/new-file content in both inputs. Workers return findings
with file/line, violated requirement, impact, and minimum fix; they never edit,
stage, publish, or change permissions. The parent owns fixes and final evidence.
If delegation is unavailable, perform both reviews locally and disclose that.

## Publish only when requested

Use `create-pr` when available for authorized publication; otherwise inspect the
intended staged diff, commit with `git -c core.hooksPath=.githooks commit`, push,
and create or update the existing PR. Do not
create a duplicate PR. A prior request to fix CI authorizes those scoped fixes;
do not ask again merely because a generic skill normally requests approval.

For authorized review replies, refresh current HEAD, threads, and checks; reply
in the original inline thread, verify the reply association, and resolve only
addressed threads. Recheck after pushing. Local passes, CI at the current SHA,
mergeability, and unresolved threads are separate facts. If the user asks to
wait for CI, monitor it through completion and fix attributable failures.
Otherwise report pending checks. Merge and release need separate authorization.

## Completion and feedback

Before delivery, inspect the final diff including new files, privacy results,
and worktree status. Report outcome, tests actually run, both review results,
and any remaining limitation. For publication, include the PR and current SHA.

When correcting a demonstrated failure, update the smallest durable home in
the same scoped change: a regression test for behavior, a specification for a
contract, this workflow/Skill for a repeated procedure, or a mechanical gate
for an objectively checkable rule. Record the trigger and validation in the PR
or final handoff. Do not copy raw history into that record or automatically
edit personal memory. Self-analysis uses an explicitly selected source scope,
then reviewed `optimize --diff`; applying suggestions still requires approval.
