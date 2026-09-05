# Analysis and findings specification

Status: foundation baseline for deterministic MVP lenses.

## 1. Analysis contract

Lenses consume canonical/store facts and emit findings. They must not read
`~/.codex`, inspect the current working tree, call a network service, or use
the current clock while evaluating a stored dataset.

Each finding contains:

```text
type
severity
confidence
scope
summary
evidence references
observed counts
suggested action
limitations
```

Evidence references point to session IDs and source paths/lines. A finding
without evidence is not emitted.

Findings are recommendations, not proof that an instruction is missing or
that a user made a mistake. Text classification is deliberately conservative.
When the evidence is ambiguous, the lens emits a lower-confidence candidate
or nothing.

## 2. Shared rules

- Normalize whitespace, case, path separators, and command wrappers only for
  matching; retain the original bounded excerpt for evidence.
- Never use a full prompt or command output as a finding key.
- Prefer structured fields such as `exit_code`, `call_id`, and timestamps over
  text guesses.
- Count both occurrences and distinct sessions.
- Do not count repeated token snapshots as repeated work.
- Keep stable ordering: severity descending, confidence descending, distinct
  session count descending, then normalized key.
- De-duplicate equivalent evidence from a paired response item and event
  completion.
- Mark missing instruction snapshots as an evidence limitation.

The initial thresholds below are deterministic defaults, not claims about all
projects. A later issue may expose configuration after real data demonstrates
that a threshold needs tuning.

| Signal | Default threshold |
| --- | --- |
| repeated | at least 2 occurrences across at least 2 sessions |
| short rework window | 10 minutes between file operations |
| stuck burst | at least 3 edits or a failure/edit loop in one window |
| excerpt | bounded and redacted before human output |

The correction lens uses only these case-insensitive marker forms after
whitespace normalization: `use ...` (with an optional trailing `instead`),
`please use ...`, `this project uses ...`, `this repo uses ...`, `this
repository uses ...`, `the project uses ...`, `this/the project requires ...`
(implemented as `this project requires`
or `the project requires`), `remember that ...`, `note that ...`,
`do not ...`, `don't ...`, and `never ...`. A question (including a trailing
`?` or a question-word prefix) and a message without one of these markers is
not a correction. The fingerprint is the bounded marker remainder with
whitespace, case, volatile IDs, and paths normalized.

The verification allowlist is intentionally small: observed `cargo` test,
nextest, fmt, clippy, check, and build; `go` test, fmt, vet, and build;
pytest, `python -m pytest`, ruff, mypy, eslint, prettier; npm/pnpm/yarn/bun
test, lint, format, and build scripts; make/just test, lint, format, build,
or check targets; and `git diff --check`. The lens does not infer a command
from the project language.

## 3. `failures`

### Input

- tool result with non-zero exit code or failed status;
- explicit error event;
- bounded stderr/output error marker when structured outcome is absent.

### Signature

`FailureSignature` is derived from tool name, normalized command family, and
normalized error category. Paths, IDs, timestamps, and line numbers are
redacted or replaced before matching.

### Finding

Emit a repeated-failure candidate when the shared threshold is met. Include:

- category and originating tool;
- distinct sessions and occurrences;
- the strongest available exit/status evidence;
- the project scope that owns the majority of occurrences, or global when no
  project has a majority;
- a proposal to document the prerequisite or preferred command.

Do not infer that a command is wrong solely because it failed once.

## 4. `corrections`

### Input

A user message that follows an assistant/tool action and contains a
correction marker or an explicit replacement instruction, for example
“use … instead”, “this project uses …”, or “do not …”.

### Rule

The first implementation may use a small, documented marker set and a
bounded normalized text fingerprint. It must keep the original role and
ordering so that a question or new requirement is not mislabeled as a
correction.

Emit a repeated-correction candidate only when a normalized correction
recurs across sessions. Include the preceding action when available.

The MVP must not infer sentiment, blame, or user intent beyond the marker and
sequence evidence.

## 5. `rework` and `stuck`

### Rework

Group file operations by session and normalized path. A path edited at least
twice within the short rework window is a rework candidate. Exclude generated
or temporary paths only when the project has an explicit instruction or a
known safe pattern; do not hard-code repository-specific exclusions.

### Stuck

A file is stuck when the same short window contains a failure/edit loop or the
stuck-burst threshold. A finding must show the sequence and avoid claiming
that repeated edits are inherently bad.

Suggested actions are scoped to the path's nearest instruction file when
known, otherwise the project root is only a candidate.

## 6. `verification`

### Input

- file operations or other evidence that code/config changed;
- subsequent tool calls classified as test, lint, format, build, or check;
- session and turn boundaries.

### Rule

Classify verification commands from structured command arguments and a small
allowlist of common command families. A project-specific command is evidence
only when observed; it is not guessed from language.

Emit a missing-verification candidate when relevant changes are followed by
session/turn completion without a recognized verification command. If the
session ended before a result was persisted, report “not observed” rather than
“not run”.

## 7. `knowledge` / `rediscovery`

The MVP treats repeated explicit facts as knowledge candidates. The first
source is repeated correction content and repeated, bounded discovery markers
that can be matched without an LLM. It does not attempt general semantic
summarization.

A candidate must include:

- the normalized fact fingerprint;
- distinct sessions;
- the source excerpts/locations;
- a destination proposal: scoped `AGENTS.md` or a docs page.

The candidate should prefer a short index/link in `AGENTS.md` when the fact is
long. “Put everything into the root file” is not a default recommendation.

## 8. `instructions`

This lens joins findings to instruction files and snapshots:

- `gap`: repeated evidence has no matching instruction text;
- `overscoped`: a finding is specific to a subtree but only a broader file
  contains the related guidance;
- `duplicate`: equivalent guidance is loaded from multiple scopes;
- `stale`: the current file differs from the snapshot associated with the
  evidence, when both are available;
- `truncated`: the effective chain reached the configured byte limit.

The lens must not declare a gap when the exact historical instruction
snapshot is unavailable; it should say that comparison is inconclusive.

## 9. `doctor`

`doctor` combines the highest-ranked findings and groups them by scope:

1. global;
2. project;
3. nearest nested instruction path.

The output is useful without a database query. It includes the analyzed
period, session count, freshness, finding counts, and a bounded evidence
sample. Machine-readable output is a later CLI issue and must keep stdout
free of progress text.

## 10. `optimize`

The initial advisor renders proposals only. A proposal must state:

- target file or scope;
- observed problem;
- evidence count and distinct sessions;
- proposed concise instruction or documentation link;
- confidence;
- why the action is scoped there;
- limitations and a review reminder.

`optimize --diff` may produce a unified diff in Phase 4. `--apply` is
intentionally excluded until safe write semantics, backups, patch validation,
and explicit user confirmation are specified.

## 11. Deterministic testing

Every lens gets small synthetic fixtures with at least one positive and one
negative case. Tests must assert the finding type, scope, counts, confidence
and evidence reference, not only a human sentence.

The same fixture and options must produce byte-for-byte stable structured
output. Real local history is never a test dependency.
