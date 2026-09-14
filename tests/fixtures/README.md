# Synthetic session fixtures

These fixtures are the only synthetic input data committed to the repository.

Rules:

- Use fictional IDs, paths, prompts, commands, timestamps, and model names.
- Keep each record small enough to inspect in a code review.
- Exercise one format behavior at a time: known records, paired tool calls,
  failures, optional fields, unknown records, and malformed-line diagnostics.
- Preserve the upstream envelope shape, but do not treat fixture values as
  production guarantees.
- Add a focused test before adding a new fixture family.
- Never copy or sanitize a real file into this directory. A synthetic fixture
  is cheaper, safer, and deterministic.

`rollout/basic.jsonl` is the smallest representative rollout. It includes an
unknown record to ensure forward compatibility is tested from the beginning.

`rollout/edge-cases.jsonl` covers thread-only identities, missing content,
structured status, unmatched tool results, and repeated token snapshots.

`rollout/session-identities*.jsonl` covers repeated rollout metadata,
main/sub-agent threads sharing one parent, and distinct thread and session
identity values.

`rollout/identityless.jsonl` covers state fallback when session metadata has no
usable identity. `rollout/state-fallback-rekey.jsonl` covers records emitted
before rollout metadata supplies the canonical identity.

`rollout/wrapper-tools.jsonl` covers direct shell calls, opaque wrapper calls
containing nested shell/patch source, and non-shell wrapper failures.

`rollout/tool-result-envelopes.jsonl` covers parsed renderer success, failure,
timeout, malformed envelopes, unknown renderer text, and incidental failure
words in successful output.

`rollout/renderer-doctor.jsonl` covers a refresh-to-doctor run with repeated
opaque renderer payload text.

`rollout/string-encoded-commands.jsonl` covers JSON-encoded function and custom
tool arguments, file patches, malformed input, and wrapper-like text.

`state/` contains synthetic SQL schemas for state adapter and store migration
tests, including separate main/sub-agent identity metadata and empty identity
fallbacks. `store/` contains synthetic rollout input and a version-one schema.

`rollout/defensive.jsonl` covers optional envelope fields, unknown nested
events, and a malformed line.

`rollout/coverage-limitations.jsonl` covers a bounded incomplete turn and a
valid sibling event for coverage-reporting tests.

Observed-instruction snapshot tests generate bounded synthetic text in memory,
so instruction content is not stored in repository fixtures.

`discovery/` contains path-only fixtures for input discovery tests. Tests copy
it into temporary directories before removing inputs or adding symlinks. The
compressed reader tests generate bounded synthetic zstd bytes in memory or in
temporary files rather than committing a binary fixture.
