# Changelog

All notable changes to the pi Rust port are documented in this file.

## [Unreleased]

### Added

- Release archives stage the repository docs tree, README.md, and CHANGELOG.md, with per-file SHA-256 digests recorded in release.json [#111]
- Release-path CHANGELOG gate: every release build (dry-run and full) fails when the root CHANGELOG.md is missing or its Unreleased section is empty [#111]
- Terminal capability overrides for hyperlinks, inline images, and true color via `PI_HYPERLINKS`, `PI_IMAGE_PROTOCOL`, and `PI_TRUE_COLOR` environment variables and `terminal.hyperlinks`, `terminal.images`, and `terminal.trueColor` JSON settings, with live reload that replaces only those three capabilities.

### Fixed

- Large tool results that cross the context threshold now compact before the next assistant request in the same run. Compaction preserves queued steering and stops cleanly when the run is cancelled or terminated.
- Terminal resizing now keeps the inline viewport aligned with the rendered frame and restores its requested height when the terminal grows.
