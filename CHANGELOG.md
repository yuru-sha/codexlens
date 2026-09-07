# Changelog

Release notes for `codexlens` source releases.

## [0.1.0] - Unreleased

The first release candidate for the local, deterministic MVP.

### Implemented

- Local Codex state and rollout ingestion with instruction capture and
  evidence-backed analysis lenses.
- Human-readable reporting over the derived SQLite store, including the
  `doctor` and `optimize --diff` workflows.
- Compressed rollout readers for `.jsonl.zst` inputs.
- Explicit `refresh` and `--frozen` reporting; reporting never refreshes
  implicitly.
- Versioned `--format json` output for supported reporting commands.
- Bounded local `monitor` support for rollout and state sources.
- Explicitly confirmed `optimize --apply --yes`, with validated write sets,
  retained backups, and whole-batch recovery on failure.

### Known verification limits

- Rust 1.85.0 and 1.92.0 are checked on Ubuntu in CI; macOS is the required
  development platform and is checked on `macos-latest`.
- Windows is checked on `windows-latest` for build and test compatibility.
  Windows runtime behavior, packaging, and installers are not release claims.
- The release is a source release. No package-manager publishing or hosted
  service is required.
