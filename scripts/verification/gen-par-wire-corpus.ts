#!/usr/bin/env bun
/**
 * PAR-WIRE fixture corpus generator (issue #30).
 *
 * Derives the golden remote-session wire corpus from the pinned upstream
 * `.references/pi-2.0/packages/protocol/src` at the canonical identity.
 * Offline-deterministic: the upstream encoder is invoked over fixed messages;
 * outputs are hex records.
 *
 * The structural declarations below mirror the pinned canonical schema in
 * `.references/pi-2.0/packages/protocol/src/schemas.ts` (SHA
 * 853a80d26c90a14c1886f0ebb8ffaae133ca2185). They are declared here rather
 * than imported because the reference checkout is untracked and N2 forbids
 * static type imports into `.references`; runtime framing and encoding still
 * come from the pinned checkout, but only after its identity has been verified.
 *
 * COMPATIBILITY NOTE: the current Rust product's
 * `crates/pi/src/remote/schemas.rs` still declares wire protocol version 8 with
 * a different ClientMessage/ServerMessage universe: a request envelope carrying
 * `target` + `call`, a `cancel` envelope, a `response` with optional/null
 * result, a `service_update` envelope, and an `attachment` envelope. Those
 * v8-only shapes are absent from the pinned v1 reference and are intentionally
 * omitted below rather than fabricated.
 *
 * Corpus shape (packages/pi-remote-protocol/tests/fixtures/par-wire-corpus.jsonl):
 *   { kind, message?, frameHex, note }
 *
 * `bun run gen:par-wire-corpus` regenerates; the diff must be empty.
 */

import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { assertCanonicalReference, canonicalReferenceRoot } from "../reference-identity.ts";

type JsonValue =
	| null
	| boolean
	| number
	| string
	| JsonValue[]
	| { [key: string]: JsonValue };

type Identifier = string;
type ServerId = Identifier;

type ProtocolErrorCode =
	| "version"
	| "busy"
	| "session_locked"
	| "not_found"
	| "invalid_request"
	| "not_implemented"
	| "internal_error";

interface ProtocolError {
	code: ProtocolErrorCode;
	message: string;
	details?: JsonValue;
}

type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";

interface ModelCost {
	input: number;
	output: number;
	cacheRead: number;
	cacheWrite: number;
}

interface ModelMetadata {
	provider: Identifier;
	id: Identifier;
	name: string;
	api: Identifier;
	reasoning: boolean;
	input: Array<"text" | "image">;
	contextWindow: number;
	maxTokens: number;
	cost: ModelCost;
	supportedThinkingLevels: ThinkingLevel[];
	authenticated: boolean;
}

interface SessionMetadata {
	id: Identifier;
	createdAt: number;
	updatedAt?: number;
	parentSessionId?: Identifier;
	sessionName?: string;
	cwd?: string;
}

interface ServerSnapshot {
	serverId: ServerId;
	protocolVersion: 1;
	revision: number;
	sessions: SessionMetadata[];
	models: ModelMetadata[];
}

interface ListResult {
	command: "list";
	sessions: SessionMetadata[];
}

type ServerEvent =
	| { type: "server_snapshot"; snapshot: ServerSnapshot }
	| { type: "session_snapshot"; snapshot: ServerSnapshot }
	| { type: "session_progress"; sessionId: Identifier; progress: JsonValue }
	| { type: "session_removed"; sessionId: Identifier };

interface ClientHello {
	type: "hello";
	version: number;
}

type ClientMessage = ClientHello;

interface ServerHello {
	type: "hello";
	version: 1;
	connectionId: Identifier;
	snapshot: ServerSnapshot;
}

interface ServerHelloError {
	type: "hello_error";
	error: ProtocolError;
}

interface ResponseOk {
	type: "response";
	id: Identifier;
	ok: true;
	result: ListResult;
}

interface ResponseError {
	type: "response";
	id: Identifier;
	ok: false;
	error: ProtocolError;
}

interface EventEnvelope {
	type: "event";
	event: ServerEvent;
}

type ServerMessage = ServerHello | ServerHelloError | ResponseOk | ResponseError | EventEnvelope;

type WireMessage = ClientMessage | ServerMessage;

const here = dirname(fileURLToPath(import.meta.url));
const upstreamRoot = join(canonicalReferenceRoot(), "packages/protocol/src");

// Exception for ts-no-dynamic-import: the module paths are only known after
// the canonical checkout has been identity-verified, and the gate must run
// before any reference read. Static imports would bypass that guard and
// hard-code an untracked path.
assertCanonicalReference();
const { FrameDecoder, DEFAULT_MAX_FRAME_LENGTH } = await import(join(upstreamRoot, "framing.ts"));
const { encodeClientMessage, encodeServerMessage } = await import(join(upstreamRoot, "codec.ts"));
const { PROTOCOL_VERSION } = await import(join(upstreamRoot, "schemas.ts"));

// --- Client message witnesses ---

const clientHello: ClientMessage = { type: "hello", version: PROTOCOL_VERSION };

// --- Server message witnesses ---

const serverId: ServerId = "server-1";
const connectionId: Identifier = "connection-1";
const sessionId: Identifier = "session-1";

const emptyServerSnapshot: ServerSnapshot = {
	serverId,
	protocolVersion: 1,
	revision: 0,
	sessions: [],
	models: [],
};

const serverHello: ServerMessage = {
	type: "hello",
	version: PROTOCOL_VERSION,
	connectionId,
	snapshot: emptyServerSnapshot,
};

const serverHelloError: ServerMessage = {
	type: "hello_error",
	error: { code: "version", message: "unsupported protocol version" },
};

const responseOk: ServerMessage = {
	type: "response",
	id: "req-1",
	ok: true,
	result: { command: "list", sessions: [] },
};

const responseError: ServerMessage = {
	type: "response",
	id: "req-2",
	ok: false,
	error: { code: "session_locked", message: "session is locked" },
};

const eventEnvelope: ServerMessage = {
	type: "event",
	event: { type: "session_removed", sessionId },
};

// --- Row contract shared with codec test owner ---

interface Row {
	kind: string;
	message?: WireMessage;
	frameHex: string;
	note: string;
}

/** Encodes and records a client message row. */
function clientRow(kind: string, message: ClientMessage, note: string): Row {
	const frame = encodeClientMessage(message);
	return { kind, message, frameHex: Buffer.from(frame).toString("hex"), note };
}

/** Encodes and records a server message row. */
function serverRow(kind: string, message: ServerMessage, note: string): Row {
	const frame = encodeServerMessage(message);
	return { kind, message, frameHex: Buffer.from(frame).toString("hex"), note };
}

// The corpus intentionally covers only the pinned v1 message universe.
// The following v8-only cases from the legacy generator are incompatible with
// the canonical reference and are therefore omitted rather than fabricated:
//   - ClientMessage request envelope with `target` + `call`
//   - ClientMessage cancel envelope
//   - ServerMessage response with `ok=true` and a null or omitted result
//   - ServerMessage service_update envelope
//   - ServerMessage attachment envelope (null or live session route)

const rows: Row[] = [
	clientRow("client_hello", clientHello, "ClientMessage hello, protocol v1"),
	serverRow("server_hello", serverHello, "ServerMessage hello with empty snapshot"),
	serverRow("server_hello_error", serverHelloError, "hello_error with version code"),
	serverRow("response_ok", responseOk, "response envelope ok=true list result"),
	serverRow("response_error", responseError, "response envelope ok=false session_locked"),
	serverRow("event_envelope", eventEnvelope, "event envelope session_removed"),
];

// --- Frame-bound rejection witness: declared length exceeds 16 MiB limit ---

{
	const huge = 16 * 1024 * 1024 + 1;
	const prefix = Buffer.alloc(4);
	prefix.writeUInt32BE(huge, 0);
	const decoder = new FrameDecoder();
	let rejected = false;
	try {
		decoder.push(new Uint8Array(prefix));
	} catch {
		rejected = true;
	}
	if (!rejected) throw new Error("over-limit frame was not rejected");
	if (DEFAULT_MAX_FRAME_LENGTH !== 16 * 1024 * 1024) {
		throw new Error(`unexpected DEFAULT_MAX_FRAME_LENGTH ${DEFAULT_MAX_FRAME_LENGTH}`);
	}
	rows.push({
		kind: "over_limit_rejection",
		frameHex: prefix.toString("hex"),
		note: `prefix declares ${huge} bytes; decoder rejects at ${DEFAULT_MAX_FRAME_LENGTH}`,
	});
}

const target = join(here, "../../packages/pi-remote-protocol/tests/fixtures/par-wire-corpus.jsonl");
const body = rows.map((r) => JSON.stringify(r)).join("\n") + "\n";
writeFileSync(target, body);
process.stdout.write(`PAR_WIRE_CORPUS_OK rows=${rows.length}\n`);
