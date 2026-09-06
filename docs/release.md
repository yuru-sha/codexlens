# Release guide

This guide prepares the `codexlens` `0.1.0` source release and keeps the
version in `Cargo.toml`, [`CHANGELOG.md`](../CHANGELOG.md), and the release
tag `v0.1.0` consistent. The current CLI surface and examples remain in the
[README](../README.md#cli-surface).

## Supported platforms and toolchain

- macOS is the required development and local verification platform.
- Ubuntu CI runs Rust 1.85.0 and Rust 1.92.0 with formatting, clippy, and the full
  test suite.
- macOS CI runs Rust 1.85.0 with `cargo build --all-features` and
  `cargo test --all-features`.
- Windows CI runs the same build and test commands on `windows-latest`.
  Hosted-runner build/test coverage is the Windows verification boundary;
  Windows runtime behavior, packaging, and installers are not supported
  claims.

Rust 1.85 or newer is required by the package metadata. A new contributor can
build, test, and run the CLI help without a Codex installation:

```bash
cargo build --all-features
cargo test --all-features
cargo run -- --version
cargo run -- --help
```

For a local report, first create a derived store with the explicit `refresh`
workflow described in the [README CLI surface](../README.md#cli-surface), then
use the reporting commands against that store.

## Current CLI boundaries

- `refresh` is the explicit raw-input workflow and writes only the selected
  derived store; raw rollout/state and instruction inputs remain read-only.
- Reporting commands read the derived store, never refresh implicitly, and
  support `--frozen` and opt-in `--format json` output.
- `monitor` is an explicit local polling workflow; it updates the derived
  store and optional cursor file without modifying its observed source.
- `optimize --diff` is review-only. `optimize --apply --yes` is the only
  product write exception and updates only its validated instruction/
  documentation write set, retaining backups and recovering the batch on
  failure.
- Processing is local and deterministic; no hosted service or LLM is needed.

## Release checklist

- [ ] Set the package version in `Cargo.toml` and the matching changelog entry;
      keep the release tag as `v0.1.0`.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --all-targets --all-features -- -D warnings`.
- [ ] Run `cargo build --all-features`.
- [ ] Run `cargo test --all-features` and the documented CLI examples.
- [ ] Review deterministic, privacy, source read-only, and apply recovery
      evidence in [`final-audit.md`](readiness/final-audit.md).
- [ ] Confirm `README.md`, [`CHANGELOG.md`](../CHANGELOG.md), this guide, and
      the CLI documentation describe the same current behavior.
- [ ] Confirm the MIT [`LICENSE`](../LICENSE) and README Inspiration
      attribution are present and accurate.
- [ ] Review `git diff --check`, the staged file list, and the published source
      commit for unrelated changes or private data.

## Minimal source-release procedure

1. Complete the checklist on the merged `main` commit.
2. Create the annotated tag: `git tag -a v0.1.0 -m "codexlens v0.1.0"`.
3. Publish the tag: `git push origin v0.1.0`.
4. Create the GitHub release from `v0.1.0` and link this changelog. GitHub's
   source archive for the tag is the distribution artifact.

This procedure deliberately stops at a source archive: there is no
Homebrew/Scoop or other package-manager publishing step.
