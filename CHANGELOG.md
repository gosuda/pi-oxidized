# Changelog

All notable changes to the pi Rust port are documented in this file.

## [Unreleased]

### Added

- Release archives stage the repository docs tree, README.md, and CHANGELOG.md, with per-file SHA-256 digests recorded in release.json [#111]
- Release-path CHANGELOG gate: every release build (dry-run and full) fails when the root CHANGELOG.md is missing or its Unreleased section is empty [#111]
