# CI
PRs and merge queues keep Linux GNU release verification, the unit/host/product
compatibility tiers and one combined documentation/snippet/doctest job.
The full seven-target release matrix, performance tier and release compatibility
tier run weekly, manually and for v* tags. Duplicate main-push verification is
removed. This reduces routine execution from fourteen jobs to five.

## Platform and provenance
Every runner is Depot. Linux uses Ubuntu 24.04; Windows retains the pinned
2025 image family. All macOS jobs use depot-macos-latest.
The Intel macOS row uses x64 Rust, Bun and Node on Apple Silicon through
Rosetta and records that execution mode. It is not a physical Intel Mac
validation. ARM64 rows retain their fail-closed native-host and no-QEMU checks.
All seven artifact formats, both determinism passes, archive and interaction
checks, Windows ConPTY evidence, signature verification and fork attestation
guards remain. Attested packaging does not restore the shared Rust cache;
Depot cache storage does not provide GitHub's branch isolation.
