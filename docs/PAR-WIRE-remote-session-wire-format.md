# PAR-WIRE: remote-session wire format decision

Stable ID: `PAR-WIRE` · Issue #30 · Owner: `pi::remote` (ledger rows R1–R4)

## Decision

The remote session stack (R1–R4) ports the upstream `pi-protocol` wire exactly: strict RFC 8949 CBOR payloads inside the 4-byte big-endian length-prefixed frame (`DEFAULT_MAX_FRAME_LENGTH` 16 MiB), with configurable decoder limits, `ServerSnapshot`/`ProtocolError` codes, and protocol version 1. The landed JSONL framing is **not** carried into R1–R4.

C4 (`modes/rpc`, JSONL over stdio) is a different, already-landed surface. This decision does not reopen C4 and does not alter `modes/rpc`.

## Evidence

- Upstream authority `.references/pi/packages/protocol/src/framing.ts:1-165`: `FRAME_HEADER_LENGTH = 4`, `DEFAULT_MAX_FRAME_LENGTH = 16 * 1024 * 1024`, big-endian `length >>> 24/16/8/0` prefix, and `FrameDecoder` (lines 58-165) with incremental split and configured limit rejection.
- Upstream `cbor/encoder.ts` + `cbor/decoder.ts` + `cbor/options.ts` (436 lines): strict RFC 8949 encode/decode with configurable limits (`Options` carries map/array/string/depth bounds), not a permissive codec.
- Upstream `schemas.ts:260-450`: full message universe pinned — `ClientMessage` (hello + request envelope), `ServerMessage` (hello, hello_error, response envelope ok/error, event envelope), `Command`/`CommandResult` union, `ServerEvent` (server_snapshot, session_snapshot, session_progress, session_removed), `ServerSnapshot` (`protocolVersion: 1`, `revision`, sessions, models), `ProtocolErrorCode` ∈ {version, busy, session_locked, not_found, invalid_request, not_implemented, internal_error}.
- Upstream consumer `packages/client/src/client.ts:1-13` imports `encodeClientMessage`, `Command`, `CommandResult`, `ResponseEnvelope`, `ServerEvent`, `ServerSnapshot` from `@earendil-works/pi-protocol` over `ByteTransport` (`transport.ts:1-18`): the TS client interop requirement binds to the CBOR+frame stack, not JSONL.
- Upstream `packages/coding-agent/src/modes/rpc/` (rpc-mode, rpc-client, rpc-types, jsonl; 1,785 lines) is the JSONL surface and it does **not** import `pi-protocol`; JSONL and the CBOR remote stack are disjoint upstream surfaces.
- Landed C4 port `crates/pi/src/modes/rpc/jsonl.rs:1-29` and the six-file `modes/rpc/` tree (8,096 lines, `types.rs` is 3,025) mirror `modes/rpc` exactly. The distinct `pi::remote` CBOR stack is now implemented under `crates/pi/src/remote/`; ledger rows R1–R4 record its codec, schemas, client transport, and server endpoint.

## Rejected option: JSONL for R1–R4

- Breaks TS-client interop: the upstream `client` package speaks CBOR-in-frames over `ByteTransport`; a JSONL R1–R4 could not talk to it without a second, forked client.
- Loses the 16 MiB frame bound and the configurable CBOR depth/map/array/string limits; JSONL self-delimits by line, so a hostile peer can stream an unbounded record and the only defenses would be hand-rolled.
- 4-byte BE prefix overhead is fixed and tiny (4 bytes per message); encode/decode cost is the same complexity class as `serde_json` (single-pass, schema-directed), so JSONL buys no measurable win.
- Golden-fixture testability is equal or worse: upstream fixtures are framed CBOR bytes; a JSONL corpus would be a new invention with no upstream witness.

## Downstream contract for R1–R4 (binding)

- R1 `remote/codec.rs`, `remote/framing.rs`: port `framing.ts` + `cbor/*` semantics byte-exactly (BE u32 prefix, 16 MiB default, configurable limits).
- R2 `remote/schemas.rs`: mirror `schemas.ts` universe including all seven `ProtocolErrorCode` literals and `protocolVersion = 1`.
- R3/R4: `ByteTransport`-shaped client, `#[cfg(unix)]` adapter as the ledger already records.
- Fixture corpus (landed with this decision): `scripts/verification/gen-par-wire-corpus.ts` regenerates `packages/pi-remote-protocol/tests/fixtures/par-wire-corpus.jsonl` offline-deterministically by invoking the pinned upstream encoder at `.references/pi` (8fa7eeb). Seven rows: client hello, server hello, hello_error, response ok/error, event envelope, and an over-limit frame rejection. R1's codec tests must decode every row byte-exactly; regeneration diff must be empty.
