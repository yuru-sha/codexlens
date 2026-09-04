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

`discovery/` contains path-only fixtures for input discovery tests. Tests copy
it into temporary directories before removing inputs or adding symlinks. The
`.jsonl.zst` file only verifies reader selection in this phase and is not
decompressed here.
