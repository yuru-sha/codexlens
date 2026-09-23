# cclens command-contract fixture

Synthetic Claude Code transcripts built from the public parser contract in cclens `src/adapter/transcript.rs` at commit `3df5f76eb14a53c4cb03d975fd4dbd4eb2f7cc70`: user turns, `assistant` `tool_use`, linked `user` `tool_result`, Skill invocations, timestamps, and usage fields. Synthetic Skills follow `src/adapter/config.rs` frontmatter rules. The fixture has no real prompts, paths, or session data.

Signals: a Skill invocation; four rapid edits to one path; repeated project-owned `cargo test` failures; the same missing-tool failure in two projects; and steer/correct/question/instruct prompts. These are paired by meaning with `tests/fixtures/analysis/command-contract.jsonl` for codexlens.
