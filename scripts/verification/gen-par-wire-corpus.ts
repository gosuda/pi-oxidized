#!/usr/bin/env bun
/**
 * PAR-WIRE fixture corpus generator (issue #30).
 *
 * Derives the golden remote-session wire corpus from the pinned upstream
 * `.references/pi` protocol package at 8fa7eeb. Offline-deterministic: the
 * upstream encoder is invoked over fixed messages; outputs are hex records.
 *
 * Corpus shape (packages/pi-remote-protocol/tests/fixtures/par-wire-corpus.jsonl):
 *   { kind, message?, frameHex, note }
 *
 * `bun run gen:par-wire-corpus` regenerates; the diff must be empty.
 */

import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const upstreamRoot = join(here, "../../.references/pi/packages/protocol/src");

const { FrameDecoder, DEFAULT_MAX_FRAME_LENGTH } = await import(join(upstreamRoot, "framing.ts"));
const { encodeClientMessage, encodeServerMessage } = await import(join(upstreamRoot, "codec.ts"));
const { PROTOCOL_VERSION } = await import(join(upstreamRoot, "schemas.ts"));

const emptyServerSnapshot = {
	serverId: "server-1",
	protocolVersion: PROTOCOL_VERSION,
	revision: 0,
	sessions: [],
	models: [],
};

const clientHello = { type: "hello", version: PROTOCOL_VERSION };
const serverHello = {
	type: "hello",
	version: PROTOCOL_VERSION,
	connectionId: "connection-1",
	snapshot: emptyServerSnapshot,
};
const serverHelloError = {
	type: "hello_error",
	error: { code: "version", message: "unsupported protocol version" },
};
const responseOk = {
	type: "response",
	id: "req-1",
	ok: true,
	result: { command: "list", sessions: [] },
};
const responseErr = {
	type: "response",
	id: "req-2",
	ok: false,
	error: { code: "session_locked", message: "session is locked" },
};
const eventEnvelope = {
	type: "event",
	event: { type: "session_removed", sessionId: "session-1" },
};

interface Row {
	kind: string;
	message?: unknown;
	frameHex: string;
	note: string;
}

function row(kind: string, message: unknown, encode: (value: never) => Uint8Array, note: string): Row {
	// Upstream encode*Message already returns the framed form (4-byte BE
	// prefix + CBOR payload); a second encodeFrame wrap would double-frame.
	const frame = encode(message as never);
	return { kind, message, frameHex: Buffer.from(frame).toString("hex"), note };
}

const rows: Row[] = [
	row("client_hello", clientHello, encodeClientMessage, "ClientMessage hello, protocol v1"),
	row("server_hello", serverHello, encodeServerMessage, "ServerMessage hello with empty snapshot"),
	row("server_hello_error", serverHelloError, encodeServerMessage, "hello_error with version code"),
	row("response_ok", responseOk, encodeServerMessage, "response envelope ok=true list result"),
	row("response_error", responseErr, encodeServerMessage, "response envelope ok=false session_locked"),
	row(
		"event_envelope",
		eventEnvelope,
		encodeServerMessage,
		"event envelope session_removed",
	),
];

// Over-limit rejection witness: a frame whose declared prefix exceeds the
// configured max must fail FrameDecoder, proving the 16 MiB bound is live.
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
console.log(`PAR_WIRE_CORPUS_OK rows=${rows.length}`);
