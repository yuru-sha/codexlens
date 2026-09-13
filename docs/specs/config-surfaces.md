# Codex configuration surfaces specification

The adapter owns this upstream-format knowledge. Analysis and reports consume
the normalized `Surface` record only.

## Roots and discovery

Given `CODEX_HOME`, inspect only these roots:

| Surface | Path or source | Scope |
| --- | --- | --- |
| global config | `$CODEX_HOME/config.toml` | global |
| global instructions | `$CODEX_HOME/AGENTS.md`, `$CODEX_HOME/AGENTS.override.md` | global |
| global rules | `$CODEX_HOME/rules/**/*.rules` | global |
| global skills | `$CODEX_HOME/skills/<name>/SKILL.md` | global |
| project instructions | project root and ancestors: `AGENTS.md`, `AGENTS.override.md` | project/nested |
| project skills | `<project>/.agents/skills/<name>/SKILL.md`, `<project>/.codex/skills/<name>/SKILL.md` | project |
| project config | `<project>/.codex/config.toml` when present | project |
| MCP servers | `[mcp_servers.<name>]` tables in applicable config | global/project |
| plugins | `[plugins."<name>@<marketplace>"]` tables in applicable config | global |
| hooks | `[hooks.*]` tables and `$CODEX_HOME/hooks.json` when present | global/project |

The project set comes from distinct absolute `project`/`cwd` values in selected
main sessions. Missing project directories are retained as session scopes but
contribute no live filesystem surfaces. Symlinks are resolved for identity and
must not escape the configured root when reading global data.

## Surface record

```text
Surface {
  id: stable hash(kind, resolved path, name),
  kind: instruction | rule | skill | config | mcp_server | plugin | hook,
  name: bounded display name,
  path: resolved path or null for inline config,
  scope: global | project(root) | nested(path),
  enabled: true | false | unknown,
  load_mode: startup_full | startup_description | path_conditional | on_demand | tool_schema | unknown,
  static_bytes: integer | null,
  startup_bytes: integer | null,
  observed_uses: integer,
  observed_sessions: integer,
  usage_state: unused | rare | used | unknown,
  limitations: [string]
}
```

`static_bytes` and `startup_bytes` are byte-based estimates in v1, not token
counts or billing. The report labels them as estimates. For a Skill, the
frontmatter `description` is `startup_description` and the remaining body is
`on_demand`. `AGENTS.md` and rules without a path condition are
`startup_full`; path-conditioned rules are `path_conditional`. MCP schemas are
`tool_schema` and unknown unless a schema is present in local state.

## Usage attribution

- A Skill use is an observed canonical tool/event name matching the Skill
  identifier; unmatched dynamic tool names remain `unknown`.
- MCP use is attributed by server/tool identity when both are present.
- A surface is `unused` only when the analysis window has complete usage
  evidence for that surface. Otherwise it is `unknown`.
- Project shadowing attributes a use to the narrowest matching project surface;
  global surfaces retain a separate aggregate count.
- Main sessions are selected by default. A session with a non-null parent id is
  a sub-agent and is excluded unless `--include-subagents` is passed.

## Inventory rules

`unused` is actionable only when `observed_uses = 0` and the evidence window is
complete. `rare` requires exactly one observed use across the window. A
`startup_full` surface is `heavy` when its startup byte estimate is above the
90th percentile of startup surfaces, with a minimum of 4 KiB. Ties are broken
by resolved path. No surface is recommended for removal solely from size.

## Privacy and tests

Never persist config values for secrets, environment variables, command args,
or private keys. Persist names, paths, booleans, bounded size estimates, and
hashes only. Fixtures must include one global Skill, one project rule, one MCP
server, one unused surface, and one unreadable/missing surface; tests assert
the exact inventory state and target-specific recommendations.
