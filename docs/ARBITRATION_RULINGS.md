# Arbitration rulings (MAP-2)

The fifteen arbitration rulings (AR1–AR15) resolve every ownership and edge
finding raised against the MAP-1 stable-ID DAG registry. Each ruling closes at
least one review finding, names exactly one owning sibling ticket ID, and adds
only integration edges — no sibling implementation, no witness code. The
rulings are ratified on canonical issue #12 and verified by
`bun run verify:arbitration`, which re-runs the MAP-1 graph check against the
unchanged published records and asserts every binding edge and ownership
assignment below against the live registry.

## Rulings

| Ruling | Owner | Surface | Binding edge | Rejected option |
| --- | --- | --- | --- | --- |
| AR1 | XC-2 (#41) | Mirror lockstep witness for protocol.rs, TypeScript METHODS, and frames.jsonl | PAR-CLOSE blocked_by XC-2 | Shared ownership between PAR and XC tracks |
| AR2 | PAR-CLIENT (#33) | cfg(unix) platform contract for PAR transports: Unix adapter gated, transport-neutral client plus in-memory adapter portable to windows-msvc, typed EndpointSpecError::UnsupportedOnPlatform on non-Unix, same commit | — | Portable-only (loses Unix sockets) or Unix-only (not portable) |
| AR3 | PAR-TEL (#71) | Six-site AgentLoopConfig telemetry boundary: agent.rs, config.rs, run.rs, schedule.rs, pi/src/core/agent_session/mod.rs, pi_agent_stream_frame_bench.rs | — | Unpinned AgentLoopConfig literal construction |
| AR4 | XC-1 (#52) | Extension-host endpoint ownership: pi-ext owns the endpoint; pi and pi-agent do not | — | pi or pi-agent owning the extension-host endpoint |
| AR5 | REL-DOCS (#111) | Release constants plus documentation staging under the single REL-DOCS row; DOC-F is consume-only | REL-CLOSE blocked_by REL-DOCS; DOC-F blocked_by REL-DOCS, REL-CLOSE, and DOC-F's existing closure prerequisites | DOC track owning release docs |
| AR6 | PAR-COMPAT-DISPO (#45) | Config-value: one parser and one command cache (A7); dead config_value wrapper disposed | — | Split parser/cache across crates |
| AR7 | XC-2 (#41) | XC mirror witness precedes PAR ratification | PAR-CLOSE blocked_by XC-2 | Parallel or after PAR closure |
| AR8 | DOC-A (#129) | Doc-evidence ledger depends on workflow reference alignment | DOC-A blocked_by VER-ALIGN | No dependency on VER-ALIGN |
| AR9 | DOC-E (#136) | CHANGELOG and release instructions consume REL constants read-only | DOC-E blocked_by REL-CLOSE | Write access or no dependency |
| AR10 | DOC-F (#138) | Publication verification depends on the dependency closing audit | DOC-F blocked_by DEPS-D1 | No dependency |
| AR11 | TUI-V1 (#76) | TUI state-matrix verification depends on the release platform definition | TUI-V1 blocked_by EXT-26 | No dependency |
| AR12 | TUI-V1 (#76) | TUI state-matrix verification depends on the portable PTY harness | TUI-V1 blocked_by TUI-P1 | No dependency |
| AR13 | PERF-T6 (#88) | Extension-host scaling lane bound to pi_ext::server::serve_io with a deterministic NativeExtension adapter | — | Different boundary or no binding |
| AR14 | PAR-CLOSE (#39) | PAR closure precedes extension compatibility closure | XC-CLOSE blocked_by PAR-CLOSE | Parallel or before PAR closure |
| AR15 | MAP-5 (#144) | All eight track closers precede the final cross-plan gate | MAP-5 blocked_by PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, ARC-CLOSE | Partial closure or subset |

## Ownership cross-check

No surface ends with two owners:

- **Mirror witness** → XC (XC-2 solely owns the lockstep witness; PAR consumes read-only)
- **Release constants plus documentation staging** → REL under the single REL-DOCS row (DOC-F is consume-only)
- **Extension-host endpoint** → pi-ext (XC track; pi and pi-agent do not own it)
- **Config-value** → one parser and one command cache (A7; PAR-COMPAT-DISPO disposes the dead wrapper)
- **Telemetry** → PAR at six AgentLoopConfig struct-literal sites (PAR-TEL)

## Binding edges

All binding edges and the tracked canonical witness live in the immutable
generation selected by
`scripts/verification/fixtures/execution-map/current.md`. The arbitration
verifier confirms each edge against the live registry and re-runs
the MAP-1 graph check to assert acyclicity, exact canonical IDs, zero
duplicates, zero aliases, full reachability, and zero REL-DOCS bypass paths.

| Edge | Dependent | Prerequisite | Rulings |
| --- | --- | --- | --- |
| Mirror witness precedes PAR ratification | PAR-CLOSE | XC-2 | AR1, AR7 |
| Doc-evidence depends on workflow alignment | DOC-A | VER-ALIGN | AR8 |
| Release docs consume REL constants | DOC-E | REL-CLOSE | AR9 |
| Publication verification consumes REL constants | DOC-F | REL-CLOSE, REL-DOCS | AR5 |
| Publication verification depends on dependency closure | DOC-F | DEPS-D1 | AR10 |
| TUI V-proof depends on release platform definition | TUI-V1 | EXT-26 | AR11 |
| TUI V-proof depends on portable transcript harness | TUI-V1 | TUI-P1 | AR12 |
| REL-CLOSE depends on REL-DOCS | REL-CLOSE | REL-DOCS | AR5 |
| PAR closure precedes XC closure | XC-CLOSE | PAR-CLOSE | AR14 |
| All eight closers precede MAP-5 | MAP-5 | PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, ARC-CLOSE | AR15 |
