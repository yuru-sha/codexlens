---
name: verify-codexlens
description: Complete scoped CodexLens implementation or PR remediation through verification, independent Standards and Spec review, and publication when requested. Use for changes and fixes, not read-only diagnosis. Writes evidence to `artifacts/<skill-name>/<RUN_ID>/`.
---

# CodexLens verify

Read [the repository workflow](../../../docs/development/workflow.md) and follow
the branch matching the user's request. Its scope, completion, review-worker,
and feedback rules are the single source of truth.

Reuse available `tdd`, `code-review`, and `create-pr` skills for their matching
steps; the workflow also defines the fallback when personal skills are absent.
Prior authorization remains valid across repair iterations. Finish all scoped
local work before requesting any genuinely new publication authority.

## Evidence output

Write verification evidence (test runs, review reports, diffs, scripts
outputs) under `artifacts/verify-codexlens/<RUN_ID>/` so runs are isolated
and discoverable. Use the same `<RUN_ID>` consistently across a single
verification cycle.