# Development environment and permissions

## Setup and verification

Install Git, Rust through rustup, Python 3.9+ (standard library only), and a C
toolchain for bundled SQLite/zstd. On macOS use Xcode Command Line Tools; on
Windows use Visual Studio C++ Build Tools and Git Bash. Codex and `gh` are
optional for building the product. GitHub operations use `gh`; authenticate
only when that workflow is requested.

The scripts default to `python3`; set `PYTHON=python` when that is the Python 3
command on your machine, as the Windows CI lane does. The commit hook uses the
same override. PyYAML is only needed for the optional external Skill validator,
not for repository gates or the Rust product.

`rust-toolchain.toml` pins local development. On a new machine, rustup may need
network access to install that toolchain; `cargo fetch --locked` populates the
dependency cache. These are explicit setup operations, not reasons to enable
network access for every agent command. No real Codex home is needed for tests.

```sh
sh scripts/verify.sh
```

This runs privacy regressions, working-tree and index scans, whitespace checks,
formatting, clippy, build, and all-feature tests. `platform` runs the same
privacy checks, build, and tests without fmt/clippy. CI sets `RUSTUP_TOOLCHAIN`
explicitly so the local pin does not override its MSRV matrix. To reproduce
the MSRV lane after installing its toolchain:

```sh
RUSTUP_TOOLCHAIN=1.85.0 sh scripts/verify.sh
```

The Rust gates use `--locked` except formatting. Cached verification can use
`CARGO_NET_OFFLINE=true`; missing dependencies are a setup failure, not a pass.
When RTK is configured, run `rtk proxy sh scripts/verify.sh` to preserve exit
codes and arguments. The repository does not require RTK on CI or other hosts.

## Privacy checks and commit hook

Keep analysis outputs under `.codexlens/`; the default `.codexlens.sqlite` and
its sidecars are also ignored. Synthetic databases remain under `tests/fixtures`.
`scripts/check_privacy.py` scans tracked and nonignored new files; `--staged`
reads actual index blobs, including forced-added ignored files. It rejects
databases outside fixtures, credential/env artifacts, home paths in fixtures,
and representative GitHub/OpenAI/AWS/private-key patterns. Findings print only
escaped paths and rule IDs. Links and files over 8 MiB fail for explicit review.

This bounded scanner is not a comprehensive secret detector: it cannot establish
fixture provenance, decode arbitrary compressed secrets, or detect every provider
or personal identifier. Review synthetic provenance and new binary fixtures;
add a maintained scanner if broader coverage is required. Never whitelist a
real credential or paste a detected value into CI logs.

For an authorized commit, use the repository hook without changing global or
shared-worktree Git configuration:

```sh
git -c core.hooksPath=.githooks commit
```

The hook checks index privacy and whitespace. It does not run the full build or
replace CI. Hooks can be bypassed; `scripts/verify.sh` and CI remain the shared
verification gate. Existing Orca/terminal notification hooks are not test gates.

## Effective sandbox

`.codex/config.toml` requests `workspace-write`, `on-request`, and disabled shell
network access. These are project defaults for trusted projects, not mandatory
policy. CLI flags and launcher overrides have higher precedence; editing this
file cannot change an already-running session. Standard workspace-write may
also permit temporary/cache locations and does not confine all file reads.

At session start, inspect the effective permissions exposed by the client.
For diagnosis, launch `codex --sandbox read-only --ask-for-approval on-request`;
for implementation use `codex --sandbox workspace-write --ask-for-approval on-request`.
Avoid bypass flags in the Orca launcher. If the client reports full access,
keep operations within the requested scope, report the mismatch, and arrange a
new restricted session; do not claim project config fixed the live session.
Administrator-enforced restrictions belong outside the repository and require
separate host configuration. Keep allowed rules narrow; project config does not
erase existing global allow rules or revoke connector permissions.

After a Codex or launcher update on macOS, run:

```sh
python3 -B scripts/check_sandbox.py
```

The script uses the installed `codex sandbox` interface to verify allowed
workspace writes, rejected sibling writes, read-only rejection, and blocked
loopback connections. Its temporary files remain under `.codexlens/`; it makes
no model request and contacts no external service. Success proves those explicit
child policies only. Separately check the new agent session's effective policy.
Other platforms need their native sandbox verification; this probe is not CI
evidence for Windows enforcement.

## Capabilities and scope

The repository needs shell/Git/Cargo/Python; `gh` covers authorized tracker work.
Reuse existing TDD/review/PR skills when present; the repository delivery skill
and workflow remain usable without personal skills. No MCP/plugin installation,
Browser/Computer Use, scheduled job, or extra runtime service is required.
Use GUI tools only for an explicitly scoped GUI acceptance criterion. Do not
automatically invoke unrelated hardware, Office, hosting, or image skills.

On shared machines, review enabled connectors, global allow rules, and Hook
payload retention before processing real history. Scope any changes to this
project; do not remove another project's tools or modify global memory as part
of an implementation. Review workers are read-only as defined in the workflow.

Configuration was checked against Codex CLI 0.153.4 and official documentation
on 2026-09-07. Skills and Hooks were marked stable by that CLI; this setup enables
no Experimental/Beta feature. Avoid deprecated approval `on-failure` and removed
feature flags. Recheck installed help/features after upgrades rather than copying
old profile syntax. The configuration is not a guarantee of model entitlement.

References: [configuration precedence](https://learn.chatgpt.com/docs/config-file/config-basic),
[configuration keys](https://learn.chatgpt.com/docs/config-file/config-reference),
and [repository skill discovery](https://learn.chatgpt.com/docs/build-skills).
