# Reporting performance benchmark

This benchmark runs the derived-store reporting path through the
`doctor --format json` CLI: SQLite load, all deterministic lenses, doctor
grouping, and JSON rendering. It uses only bounded synthetic data.

Run it with:

```bash
cargo bench --locked --bench reporting
```

The dataset contains 500 sessions, 200,000 canonical records, 10,000
non-empty messages, 1,661 file operations, 1,650 instruction snapshots, and
50,000 tool-call/result pairs. Each synthetic failure result has 1,024 bytes
of bounded output. The benchmark validates that the result is one parseable
schema-version-1 JSON document with the expected session and record counts,
exercises recurring evidence from each populated lens, is at most 64 KiB, and
finishes within the 5-second target. It also runs the other read-only JSON
reporting commands against the same store. Each focused command output is
capped at 256 KiB; the doctor output keeps the stricter 64 KiB cap.

The chosen target is at most 5 seconds per reporting command on macOS arm64.
The benchmark is enforced by the pinned `macos-14` arm64 CI job with
`cargo bench --locked --bench reporting`. A non-timing benchmark failure fails
CI immediately. If a run reports only a timing failure, CI allows up to three
total attempts and requires two subsequent clean attempts before passing; each
clean attempt enforces the 5-second limit for every command. This is a
development benchmark target for that runner; it does not claim runtime or
package support for other platforms or a universal runtime guarantee under
noisy hardware.

The synthetic assertions also require observed snapshot evidence to retain its
session-scoped source association and turn fallback, and require missing or
unavailable snapshots to produce the documented inconclusive limitation. The
JSON remains bounded and uses only synthetic data; existing privacy and
read-only tests remain the source of truth for those boundaries.

| Run | Store | Result |
| --- | --- | --- |
| Baseline from issue #76 (`main` at `1e031df`) | 486 sessions, 186,430 records, about 399 MiB | `doctor --format json` exceeded 10 minutes without producing JSON and was stopped |
| Post-change, macOS arm64 26.5.1 | 500 sessions, 200,000 records, 50,000 call/result pairs | 2,735 ms; valid JSON, 4,343 bytes |
| Issue #78 change, macOS arm64 26.5.1 | 500 sessions, 200,000 records, 10,000 messages, 1,661 file operations, 1,650 instruction snapshots, 50,000 call/result pairs | `doctor --format json`: 1,960 ms; reporting commands: 643–2,028 ms; `optimize --diff`: 1,960 ms; valid JSON, 15,916 bytes |

The synthetic source paths and failure text are bounded placeholders. No real
rollout, prompt, command output, credential, or private path is used.
