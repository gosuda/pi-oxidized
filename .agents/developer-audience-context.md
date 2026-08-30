# Developer audience context

## Primary users

The primary users are experienced coding-agent users and TypeScript extension authors. They can build from source, read protocol documentation, and evaluate compatibility and performance evidence.

## Contributors

The contributor audience includes engineers who work on Rust, TypeScript, terminal UI, provider protocols, extension compatibility, and release packaging.

## First value

A new user reaches first value when they:

1. build the TypeScript extension host and the Rust `pi` binary;
2. configure a supported provider credential;
3. launch `pi` with an explicit provider and model; and
4. receive one assistant response.

## Trust requirements

Public copy must link each material claim to its authority:

- measured speed claims to `docs/performance/PERF-CLOSE-evidence.md`;
- TypeScript extension compatibility to `docs/extension-compatibility-contract.md` and the shared JSONL witness;
- terminal safety to the Rust sanitization boundary;
- release reproducibility and target support to the release documentation; and
- current limitations to the evidence file that records each limitation.

State benchmark workloads and limits with every number. Distinguish implemented behavior from planned work.

## Voice

Use direct technical language. Lead with observable behavior. Link to primary repository evidence instead of using broad marketing claims.

## Forbidden claims

Do not claim:

- a smaller installed footprint;
- complete behavior parity;
- signed or notarized macOS distribution;
- completed accessibility compliance; or
- a universal speedup across all operations.

This file guides repository and launch copy. Product code, compatibility contracts, benchmark evidence, and release documentation remain authoritative.
