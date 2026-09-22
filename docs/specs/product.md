# CodexLens product specification

Status: target MVP contract for a Codex counterpart to `cclens`.

The implementable command and adapter contracts are [`cli.md`](cli.md) and
[`config-surfaces.md`](config-surfaces.md). This document defines product
intent; those documents define inputs, outputs, and edge cases.

## 1. Product promise

`codexlens` is a local lens onto Codex usage. It reads Codex transcripts and
the configuration that shaped them, then shows where time, tokens, and effort
are being wasted and how to fix the underlying setup.

This is not a raw event counter and not merely a database-backed finding dump.
Every user-facing result must answer what happened, why it matters, where to
change the setup, and what concrete change to try.

## 2. Data and scope

The adapter reads the default Codex home, including session transcripts,
thread/state indexes, global configuration, installed Skills, MCP/tool
surfaces when discoverable, and project instruction files. CodexLens does not
modify these raw inputs; it may observe files added or changed by Codex or other
processes during live incremental analysis. Secrets are never copied into
reports.

The derived store is per-user because the analysis spans projects. Reports
separate `global` configuration from `project:<path>` configuration. Main
sessions are the default; archived sessions and sub-agent sessions require an
explicit option. Unknown or unreadable configuration is a limitation, never
evidence that a surface is unused.

## 3. Command surface

The MVP mirrors the cclens view model with Codex adapters:

| Command | Purpose |
| --- | --- |
| `doctor` | bounded health check combining the highest-impact opportunities |
| `analyze` | refresh and derive all analysis facts/views |
| `inventory` | configured surfaces by scope and observed use |
| `overhead` | always-on instruction/configuration cost and its owners |
| `usage` | most-used tools, Skills, models, prompts, subagents, and workflow surfaces |
| `waste` | ranked optimization opportunities with suggested actions |
| `failures` | recurring tool failures by normalized category and owner |
| `stuck` | repeated edit/failure loops with affected paths |
| `prompts` | steer/correct/question/instruct interaction patterns |
| `sessions` | bounded session listing and coverage metadata |
| `sql` | read-only ad-hoc queries over the derived store |
| `query` | compatibility alias for `sql` |
| `optimize` | investigate selected findings and propose concrete config/docs fixes |

Explicit `analyze` and `refresh` commands update the derived store
incrementally. Read commands consume the existing store exactly and never
refresh it; `--frozen` makes that store-only boundary explicit. `--scope global` and
`--scope project:<path>` are supported on applicable views. Human output is
screen-sized; JSON/Markdown are detailed machine/paste formats.

## 4. Configuration inventory

An inventory item has a stable path, scope, kind, load mode when known, bounded
size/token estimate, and observed-use count. The inventory joins configured
surfaces to session evidence:

- global and project `AGENTS.md` / override files;
- Codex config and rules;
- installed Skills and instruction files;
- MCP and other configured tool surfaces when discoverable;
- effective instruction chains observed by sessions.

The views must distinguish unused, rarely used, always-on-heavy, and
frequently-used surfaces. A surface with unavailable usage evidence is
reported as unknown.

## 5. Analysis views

The initial views are deterministic and evidence-backed:

- `failures`: recurring failures grouped by normalized tool and error
  category, assigned to a strict-majority owning project or global habit;
- `stuck`: rapid edit bursts and failure/edit loops, with a concrete affected
  path and scoped destination;
- `prompts`: user steering, correction, question, and instruction patterns;
- `overhead`: always-on configuration cost reconciled with actual usage;
- `inventory`: configured surfaces multiplied by actual usage and scope;
- `usage`: ranked tools, Skills, models, prompts, subagents, and other observed surfaces;
- `waste`: the ranked union of actionable unused/heavy/failure/stuck findings;
- `doctor`: the top bounded `waste` opportunities, known user-controlled
  overhead, and other actionable findings split global/project, with owner,
  target, evidence, action, and follow-up command;
- `optimize`: a root-cause briefing and specific proposed edits, never a
  generic instruction such as “document the prerequisite”. Unsupported or
  ambiguous configuration changes remain visible as skipped limitations.

Every opportunity contains a title, impact, confidence, target path/scope,
concrete action, occurrence and session counts, up to three evidence examples,
and limitations. Findings without evidence are not emitted.

Parser wrappers, shell snippets used to invoke Codex or the analyzer, raw
response formatting, generated paths, and truncated payload artifacts are not
valid tool names, commands, or failure categories.

## 6. Doctor output

The default output is bounded and starts with a one-screen summary:

```text
CodexLens Doctor
Scope: global + projects
Sessions: ...

Top fixes
1. <problem>
   Why: <measured impact>
   Fix: <specific target and concrete change>
   Evidence: <up to three examples>
```

Detailed rows belong to view commands, JSON, Markdown, or `sql`/`query`; the normal
doctor output must not print hundreds of findings or full prompt/tool output.

## 7. Acceptance criteria

- A first `codexlens analyze` refreshes the selected inputs, after which
  `codexlens doctor` reports from the derived store without implicit refresh.
- The default store is per-user and does not depend on the repository where
  the command is run.
- Global and project scopes are distinct.
- A synthetic recurring failure yields a normalized category, owning scope,
  and target-specific fix; it never yields `const`, `s:`, or wrapper text.
- A synthetic unused/heavy configuration surface yields its path and a
  remove/slim/re-scope recommendation.
- `inventory`, `overhead`, `usage`, `waste`, `failures`, `stuck`, `prompts`,
  `doctor`, `sql`, `query`, and `optimize` are real user-facing views, not aliases
  that merely dump the same finding list.
- Human reports are bounded and actionable; JSON/Markdown preserve the same
  target/action semantics without progress text mixed into JSON stdout.
- CodexLens does not modify the raw Codex home or project sources; concurrent
  changes made by Codex or other processes do not violate this read-only
  contract.
