#!/usr/bin/env bun
/**
 * PAR-WIRE fixture corpus generator (issues #30, #31).
 *
 * Emits the golden remote-session wire corpus natively for the Rust v8 wire
 * (`crates/pi/src/remote/schemas.rs` + `codec.rs`, `PROTOCOL_VERSION = 1`).
 * Offline-deterministic: the CBOR and framing writers below mirror the codec's
 * byte rules exactly (definite-length maps in schema field order, shortest-form
 * arguments, 0xf6 null, 0xf4/f5 bools, BE u32 length prefix), so every row
 * round-trips byte-for-byte through the Rust codec. The upstream v1 reference
 * is no longer consulted: its message universe cannot express the v8-only
 * shapes (request, cancel, null/absent results, service_update, attachment)
 * that the golden corpus must cover.
 *
 * Opaque JSON witnesses (`call`, `update`) deliberately avoid numbers: the
 * Rust codec carries JSON numbers as binary64 CBOR floats (0xfb), so integer
 * literals there would not round-trip byte-exactly.
 *
 * Corpus shape (packages/pi-remote-protocol/tests/fixtures/par-wire-corpus.jsonl):
 *   { kind, message?, frameHex, note }
 *
 * `bun run gen:par-wire-corpus` regenerates; `--check` verifies the committed
 * file is byte-fresh and exits non-zero on drift.
 */

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// --- Wire constants (mirror crates/pi/src/remote) ---

/** Native wire version; carries the upstream v8 message family unchanged. */
const PROTOCOL_VERSION = 1;

/** Default upper bound for one framed payload (framing.rs). */
const DEFAULT_MAX_FRAME_LENGTH = 16 * 1024 * 1024;

/** Canonical lowercase UUIDv4 used for every fenced route in the corpus. */
const SERVER_ID = "00000000-0000-4000-8000-000000000001";
const SESSION_ID = "session-1";
const ATTACHMENT_ID = "attachment-1";

// --- CBOR writer mirroring crates/pi/src/remote/codec.rs CborEncoder ---

type WireValue =
	| null
	| boolean
	| number
	| string
	| WireValue[]
	| { [key: string]: WireValue };

const textEncoder = new TextEncoder();

/** Writes one CBOR argument: shortest form per RFC 8949 §4.2.1 as in codec.rs. */
function writeArgument(out: number[], major: number, value: number): void {
	const prefix = major << 5;
	if (value < 24) {
		out.push(prefix | value);
	} else if (value <= 0xff) {
		out.push(prefix | 0x18, value);
	} else if (value <= 0xffff) {
		out.push(prefix | 0x19, value >>> 8, value & 0xff);
	} else if (value <= 0xffff_ffff) {
		out.push(
			prefix | 0x1a,
			value >>> 24,
			(value >>> 16) & 0xff,
			(value >>> 8) & 0xff,
			value & 0xff,
		);
	} else {
		const high = Math.floor(value / 2 ** 32);
		const low = value >>> 0;
		out.push(
			prefix | 0x1b,
			high >>> 24,
			(high >>> 16) & 0xff,
			(high >>> 8) & 0xff,
			high & 0xff,
			low >>> 24,
			(low >>> 16) & 0xff,
			(low >>> 8) & 0xff,
			low & 0xff,
		);
	}
}

function writeText(out: number[], value: string): void {
	const bytes = textEncoder.encode(value);
	writeArgument(out, 3, bytes.length);
	for (const byte of bytes) out.push(byte);
}

function writeValue(out: number[], value: WireValue): void {
	if (value === null) {
		out.push(0xf6);
	} else if (typeof value === "boolean") {
		out.push(value ? 0xf5 : 0xf4);
	} else if (typeof value === "number") {
		if (!Number.isSafeInteger(value) || value < 0) {
			throw new Error(
				`envelope integers must be non-negative safe integers: ${value}`,
			);
		}
		writeArgument(out, 0, value);
	} else if (typeof value === "string") {
		writeText(out, value);
	} else if (Array.isArray(value)) {
		writeArgument(out, 4, value.length);
		for (const item of value) writeValue(out, item);
	} else {
		// Definite-length map; object literal key order is the wire field order.
		const entries = Object.entries(value);
		writeArgument(out, 5, entries.length);
		for (const [key, item] of entries) {
			writeText(out, key);
			writeValue(out, item);
		}
	}
}

function cborEncode(value: WireValue): Uint8Array {
	const out: number[] = [];
	writeValue(out, value);
	return Uint8Array.from(out);
}

/** Prefixes a payload with its unsigned 32-bit big-endian length (framing.rs). */
function frame(payload: Uint8Array): Uint8Array {
	const out = new Uint8Array(4 + payload.length);
	new DataView(out.buffer).setUint32(0, payload.length, false);
	out.set(payload, 4);
	return out;
}

// --- Message witnesses (field order mirrors schemas.rs Serialize impls) ---

const clientHello: WireValue = { type: "hello", version: PROTOCOL_VERSION };

const request: WireValue = {
	type: "request",
	id: "req-1",
	target: { serverId: SERVER_ID, sessionId: SESSION_ID, attachmentId: ATTACHMENT_ID },
	call: { args: [], tool: "list_sessions" },
};

const cancel: WireValue = {
	type: "cancel",
	id: "req-1",
	target: { serverId: SERVER_ID },
};

const serverHello: WireValue = {
	type: "hello",
	version: PROTOCOL_VERSION,
	serverId: SERVER_ID,
};

const serverHelloError: WireValue = {
	type: "hello_error",
	error: { code: "version", message: "unsupported protocol version" },
};

const responseOk: WireValue = {
	type: "response",
	id: "req-1",
	ok: true,
	result: { command: "list", sessions: [] },
};

const responseNull: WireValue = {
	type: "response",
	id: "req-2",
	ok: true,
	result: null,
};

const responseAbsent: WireValue = {
	type: "response",
	id: "req-3",
	ok: true,
};

const responseError: WireValue = {
	type: "response",
	id: "req-2",
	ok: false,
	error: { code: "session_locked", message: "session is locked" },
};

const serviceUpdate: WireValue = {
	type: "service_update",
	subscriptionId: "sub-1",
	update: { status: "streaming" },
};

const attachmentNull: WireValue = {
	type: "attachment",
	attachment: null,
};

const attachmentSession: WireValue = {
	type: "attachment",
	attachment: { serverId: SERVER_ID, sessionId: SESSION_ID, attachmentId: ATTACHMENT_ID },
};

// --- Row contract shared with codec test owner ---

interface Row {
	kind: string;
	message?: WireValue;
	frameHex: string;
	note: string;
}

/** Encodes and records one v8 message row. */
function wireRow(kind: string, message: WireValue, note: string): Row {
	const frameHex = Buffer.from(frame(cborEncode(message))).toString("hex");
	return { kind, message, frameHex, note };
}

const rows: Row[] = [
	wireRow("client_hello", clientHello, "ClientMessage hello, protocol v1"),
	wireRow("request", request, "request envelope, session target, opaque call"),
	wireRow("cancel", cancel, "cancel envelope, server target"),
	wireRow("server_hello", serverHello, "ServerMessage hello, v8 shape with canonical serverId"),
	wireRow("server_hello_error", serverHelloError, "hello_error with version code"),
	wireRow("response_ok", responseOk, "response envelope ok=true list result"),
	wireRow("response_null", responseNull, "response envelope ok=true with explicit null result"),
	wireRow("response_absent", responseAbsent, "response envelope ok=true with result key absent"),
	wireRow("response_error", responseError, "response envelope ok=false session_locked"),
	wireRow("service_update", serviceUpdate, "service_update envelope with opaque update"),
	wireRow("attachment_null", attachmentNull, "attachment envelope, detached route"),
	wireRow("attachment_session", attachmentSession, "attachment envelope, live session route"),
];

// --- Frame-bound rejection witness: declared length exceeds 16 MiB limit ---

{
	const declared = DEFAULT_MAX_FRAME_LENGTH + 1;
	const prefix = Buffer.alloc(4);
	prefix.writeUInt32BE(declared, 0);
	rows.push({
		kind: "over_limit_rejection",
		frameHex: prefix.toString("hex"),
		note: `prefix declares ${declared} bytes; decoder rejects at ${DEFAULT_MAX_FRAME_LENGTH}`,
	});
}

const here = dirname(fileURLToPath(import.meta.url));
const target = join(here, "../../packages/pi-remote-protocol/tests/fixtures/par-wire-corpus.jsonl");
const body = rows.map((row) => JSON.stringify(row)).join("\n") + "\n";

if (process.argv.includes("--check")) {
	const current = readFileSync(target, "utf8");
	if (current !== body) {
		process.stderr.write("PAR_WIRE_CORPUS_STALE: run `bun run gen:par-wire-corpus`\n");
		process.exit(1);
	}
	process.stdout.write(`PAR_WIRE_CORPUS_FRESH rows=${rows.length}\n`);
} else {
	writeFileSync(target, body);
	process.stdout.write(`PAR_WIRE_CORPUS_OK rows=${rows.length}\n`);
}
