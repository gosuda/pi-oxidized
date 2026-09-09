#!/usr/bin/env bun
/**
 * PAR-WIRE fixture corpus generator (issue #30).
 *
 * Derives the golden remote-session wire corpus from the pinned upstream
 * `.references/pi-2.0/packages/protocol/src` at the canonical identity.
 * Offline-deterministic: the upstream encoder is invoked over fixed messages;
 * outputs are hex records.
 *
 * The message declarations below intentionally mirror the pinned protocol
 * locally. Keeping those types in tracked source lets type-aware verification
 * inspect this generator without traversing the untracked reference checkout.
 * Runtime framing and encoding still come from the pinned checkout, but only
 * after its identity has been verified.
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

interface ProtocolError {
	code: Identifier;
	message: string;
}

interface ServerTarget {
	serverId: ServerId;
}

interface SessionTarget {
	serverId: ServerId;
	sessionId: Identifier;
	attachmentId: Identifier;
}

type RpcTarget = ServerTarget | SessionTarget;

type ClientMessage =
	| { type: "hello"; version: number }
	| { type: "request"; id: Identifier; target: RpcTarget; call: JsonValue }
	| { type: "cancel"; id: Identifier; target: RpcTarget };

type ServerMessage =
	| { type: "hello"; version: number; serverId: ServerId }
	| { type: "hello_error"; error: ProtocolError }
	| {
			type: "response";
			id: Identifier;
			ok: true;
			result?: JsonValue;
	  }
	| {
			type: "response";
			id: Identifier;
			ok: false;
			error: ProtocolError;
	  }
	| { type: "service_update"; subscriptionId: Identifier; update: JsonValue }
	| { type: "attachment"; attachment: SessionTarget | null };

const here = dirname(fileURLToPath(import.meta.url));
const upstreamRoot = join(canonicalReferenceRoot(), "packages/protocol/src");

// Dynamic imports: the specifiers are only known after the canonical checkout
// has been identity-verified, and the gate must run before any reference read.
assertCanonicalReference();
const { FrameDecoder, DEFAULT_MAX_FRAME_LENGTH } = await import(join(upstreamRoot, "framing.ts"));
const { encodeClientMessage, encodeServerMessage } = await import(join(upstreamRoot, "codec.ts"));
const { PROTOCOL_VERSION } = await import(join(upstreamRoot, "protocol.ts"));

// --- Client message witnesses (v8 ClientMessage union) ---

const clientHello = { type: "hello" as const, version: PROTOCOL_VERSION };

/** RequestEnvelope with a session (session-fenced) target. */
const requestEnvelope = {
	type: "request" as const,
	id: "req-1",
	target: {
		serverId: "00000000-0000-4000-8000-000000000001",
		sessionId: "session-1",
		attachmentId: "attach-1",
	},
	call: { command: "list" },
};

/** CancelEnvelope with a serverId (server-wide) target. */
const cancelEnvelope = {
	type: "cancel" as const,
	id: "req-2",
	target: { serverId: "00000000-0000-4000-8000-000000000001" },
};

// --- Server message witnesses (v8 ServerMessage union) ---

const serverHello = {
	type: "hello" as const,
	version: PROTOCOL_VERSION,
	serverId: "00000000-0000-4000-8000-000000000001",
};

const serverHelloError = {
	type: "hello_error" as const,
	error: { code: "version", message: "unsupported protocol version" },
};

/** ok=true with result present. */
const responseOk = {
	type: "response" as const,
	id: "req-1",
	ok: true as const,
	result: { command: "list", sessions: [] },
};

/** ok=true with result explicit null. */
const responseNull = {
	type: "response" as const,
	id: "req-4",
	ok: true as const,
	result: null,
};

/** ok=true with result absent (field omitted, not null). */
const responseAbsent = {
	type: "response" as const,
	id: "req-2",
	ok: true as const,
};

/** ok=false with error present. */
const responseErr = {
	type: "response" as const,
	id: "req-3",
	ok: false as const,
	error: { code: "session_locked", message: "session is locked" },
};

/** ServiceEventEnvelope: type is "service_update", not "event". */
const serviceUpdateEnvelope = {
	type: "service_update" as const,
	subscriptionId: "sub-1",
	update: { command: "list" },
};

/** AttachmentEnvelope with attachment: null (no active route). */
const attachmentEnvelopeNull = {
	type: "attachment" as const,
	attachment: null,
};

/** AttachmentEnvelope with a live session target. */
const attachmentEnvelopeSession = {
	type: "attachment" as const,
	attachment: {
		serverId: "00000000-0000-4000-8000-000000000001",
		sessionId: "session-1",
		attachmentId: "attach-1",
	},
};

// --- Row contract shared with codec test owner ---

interface Row {
	kind: string;
	message?: unknown;
	frameHex: string;
	note: string;
}

/** Encodes and records a client message row. */
function clientRow(
	kind: string,
	message: ClientMessage,
	note: string,
): Row {
	const frame = encodeClientMessage(message);
	return { kind, message, frameHex: Buffer.from(frame).toString("hex"), note };
}

/** Encodes and records a server message row. */
function serverRow(
	kind: string,
	message: ServerMessage,
	note: string,
): Row {
	const frame = encodeServerMessage(message);
	return { kind, message, frameHex: Buffer.from(frame).toString("hex"), note };
}

const rows: Row[] = [
	clientRow("client_hello", clientHello, "ClientMessage hello, protocol v8"),
	clientRow("request", requestEnvelope, "RequestEnvelope session target, list call"),
	clientRow("cancel", cancelEnvelope, "CancelEnvelope serverId target"),
	serverRow("server_hello", serverHello, "ServerMessage hello, serverId, no snapshot"),
	serverRow("server_hello_error", serverHelloError, "hello_error with version code"),
	serverRow("response_ok", responseOk, "response ok=true with result"),
	serverRow("response_null", responseNull, "response ok=true, result null"),
	serverRow("response_absent", responseAbsent, "response ok=true, result absent"),
	serverRow("response_error", responseErr, "response ok=false, session_locked"),
	serverRow("service_update", serviceUpdateEnvelope, "service_update envelope"),
	serverRow("attachment_null", attachmentEnvelopeNull, "attachment envelope, route null"),
	serverRow("attachment_session", attachmentEnvelopeSession, "attachment envelope, live session route"),
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
