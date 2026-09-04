# Session and input format specification

Status: foundation baseline for the adapter. Upstream field names in this file
are implementation input, not part of the canonical model.

## 1. Discovery

The default Codex home is `~/.codex`. A future CLI option may override it; the
resolution order is:

1. explicit command-line path;
2. `CODEX_HOME`;
3. platform default home directory plus `.codex`.

The adapter discovers, without following symlinks outside an explicit root:

- `state_*.sqlite` directly under Codex home;
- `sessions/**/*.jsonl`;
- optionally `archived_sessions/**` when the user requests archived history;
- `.jsonl.zst` through the same reader contract when encountered.

Discovery is best-effort. A missing state database must not prevent rollout
ingestion, and a missing rollout must not prevent state metadata ingestion.

The source tree is an input, never an output. `codexlens` must not rename,
truncate, repair, or delete any source file.

## 2. Rollout JSONL envelope

Each valid line is expected to be a JSON object with a loose envelope:

```json
{
  "timestamp": "2026-01-01T00:00:00.000Z",
  "type": "response_item",
  "payload": {}
}
```

The timestamp, type, and payload may be absent or have a new shape. The
adapter must preserve the original line's source path and one-based line
number, then decode only fields it needs.

The following top-level record types are known or expected and are not an
exhaustive enum:

- `session_meta`;
- `turn_context`;
- `event_msg`;
- `response_item`;
- `compacted`;
- `world_state`.

For `event_msg` and `response_item`, `payload.type` is a second-level event or
item type. New values at either level are normal compatibility cases.

## 3. Useful event families

The adapter maps observed records to these canonical families:

| Upstream evidence | Canonical family | Required behavior |
| --- | --- | --- |
| `session_meta` | session metadata | capture identity and available run metadata |
| `turn_context` | turn context | capture turn/cwd/model/policy and optional instructions |
| message response item | message | preserve role and bounded text reference |
| tool-call response item | tool call | capture call ID, tool name, and input summary |
| tool result or completion event | tool result | correlate by call ID when present |
| token-count event | token snapshot | deduplicate repeated snapshots |
| compaction/lifecycle event | lifecycle | retain ordering and compaction markers |
| any valid unknown record | unknown record | retain raw JSON and provenance |

The adapter must not assume that every tool call has a result, that every
result has a call ID, or that a command result is represented in only one
record. Correlation is best-effort and must be visible in the canonical data.

Shell-like completion records may expose fields such as `command`, `cwd`,
`stdout`, `stderr`, `exit_code`, `duration`, or `status`. These are optional.
An exit code is stronger evidence of failure than a text heuristic; the
analysis layer must prefer it when available.

## 4. Session metadata

`session_meta.payload` may provide:

- session ID or thread ID;
- creation timestamp;
- cwd;
- originator/source/thread source;
- CLI version;
- model provider and model;
- history mode;
- base-instruction metadata.

`turn_context.payload` may provide:

- turn ID;
- cwd;
- model and reasoning effort;
- approval and sandbox policy;
- workspace roots;
- current date/timezone;
- a summary;
- injected `user_instructions` in versions that persist it.

These are optional observations. The absence of `user_instructions` does not
mean that no instructions were active.

## 5. State SQLite

`state_*.sqlite` is a local index owned by Codex. It is metadata enrichment,
not the authoritative event stream. The adapter should query only the
columns it needs and tolerate extra or missing columns.

The useful logical fields are:

- thread/session identity;
- rollout path;
- created/updated times;
- source and thread source;
- cwd and project metadata;
- model/provider/reasoning metadata;
- title/preview;
- archive state;
- parent/child relationship when available.

The database may be split across multiple `state_*.sqlite` files. The
canonical session identity is the stable thread/session ID, not the database
filename. If metadata conflicts with a rollout's `session_meta`, keep the
conflict as a diagnostic and apply the documented precedence rule once the
state adapter issue defines it; do not silently overwrite evidence.

## 6. Reader abstraction

The parser consumes a reader that yields `(line_number, bytes)`:

- `PlainJsonlReader`: required first;
- `ZstdJsonlReader`: supported through the same boundary when compressed
  rollout files are encountered.

Compression support must not leak into normalization or lenses. It is an I/O
choice. The initial foundation does not add a compression dependency until
the reader issue needs it.

Large lines must be streamed. The adapter may enforce a configurable maximum
line size and report an oversized-line diagnostic rather than allocating
unbounded memory.

## 7. Error and drift policy

| Condition | Default behavior |
| --- | --- |
| unreadable source file | warning diagnostic; continue other files |
| invalid JSON line | diagnostic with file/line/reason; continue |
| valid JSON with unknown record type | retain as `UnknownRecord` |
| missing optional field | use unknown/null; continue |
| duplicate source file | skip using `ingested_files` identity |
| changed source file | re-ingest according to file identity policy |
| state schema mismatch | query compatible columns and record a diagnostic |

Strict failure mode can be added later for CI or fixture validation. Normal
local analysis should prefer partial, explicit results to an all-or-nothing
run.

## 8. Provenance and storage

Every canonical record carries:

- source kind (rollout or state);
- source path;
- one-based source line for JSONL, when applicable;
- original record type;
- event timestamp, if parseable;
- ingest timestamp;
- parser/schema version.

Known content is stored only when a lens needs it. Unknown valid records keep
their raw JSON in the local SQLite store, not in repository fixtures or
reports by default.

## 9. Synthetic coverage

Fixture families must cover:

1. minimal session metadata and turn context;
2. user/assistant messages;
3. a tool call paired with a result;
4. an explicit non-zero exit code;
5. token snapshots;
6. repeated optional fields;
7. unknown top-level and nested event types;
8. malformed JSON and oversized-line diagnostics;
9. instruction-chain snapshots.

See [`tests/fixtures/README.md`](../../tests/fixtures/README.md). The
representative fixture is intentionally fictional and small.

## 10. References

- [OpenAI Codex rollout module](https://github.com/openai/codex/tree/main/codex-rs/rollout)
- [cclens architecture](https://github.com/lambdalisue/cclens/blob/main/docs/specs/architecture.md)
- [codex-session-insights input description](https://github.com/cosformula/codex-session-insights/blob/main/README.md)
