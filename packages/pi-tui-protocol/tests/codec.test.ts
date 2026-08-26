import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
	decodeFrameStr,
	decodeFrameStrStrict,
	encodeFrame,
	encodeFrameString,
	FrameDecoder,
	ProtocolError,
	requestFrame,
} from "../src/codec.ts";
import {
	COMPATIBILITY_VERSION,
	isMethod,
	localHello,
	MAX_FRAME_BYTES,
	METHODS,
	PROTOCOL_VERSION,
} from "../src/types.ts";

const fixturesPath = join(
	dirname(fileURLToPath(import.meta.url)),
	"fixtures",
	"frames.jsonl",
);

describe("constants", () => {
	test("versions and size", () => {
		expect(PROTOCOL_VERSION).toBe(1);
		expect(COMPATIBILITY_VERSION).toBe("0.80.10");
		expect(MAX_FRAME_BYTES).toBe(8 * 1024 * 1024);
	});

	test("extension control methods are allowlisted", () => {
		for (const method of ["flags.set", "shortcut.execute"] as const) {
			expect(METHODS).toContain(method);
			expect(isMethod(method)).toBe(true);
			const frame = requestFrame(2, method, {});
			expect(decodeFrameStrStrict(encodeFrameString(frame).trimEnd())).toEqual(frame);
		}
	});

	test("session.* open methods are rejected by strict decoding (Rust Method::parse parity)", () => {
		// Rust treats session.* as open method strings (const &str), not Method
		// enum variants — Method::parse returns None for them, so strict decoding
		// (decode_frame_str_strict) rejects them. TypeScript must match.
		const sessionMethods = [
			"session.newSession", "session.fork", "session.navigateTree",
			"session.switchSession", "session.reload", "session.replacementReady",
			"session.replacementAbort",
		] as const;
		for (const method of sessionMethods) {
			expect(METHODS).not.toContain(method);
			expect(isMethod(method)).toBe(false);
			const line = JSON.stringify({ id: 1, kind: "req", method, payload: {} });
			expect(decodeFrameStr(line).method).toBe(method);
			expect(() => decodeFrameStrStrict(line)).toThrow(ProtocolError);
		}
	});
});

describe("encode/decode", () => {
	test("hello roundtrip", () => {
		const frame = requestFrame(1, "hello", localHello());
		const line = encodeFrameString(frame);
		expect(line.endsWith("\n")).toBe(true);
		const decoded = decodeFrameStr(line.trimEnd());
		expect(decoded).toEqual(frame);
	});

	test("id rules", () => {
		expect(() => encodeFrame({ id: 0, kind: "req", method: "hello", payload: {} })).toThrow(
			ProtocolError,
		);
		expect(
			decodeFrameStr(
				JSON.stringify({ id: 0, kind: "event", method: "notify", payload: {} }),
			).id,
		).toBe(0);
	});

	test("strict unknown method", () => {
		const line = JSON.stringify({
			id: 1,
			kind: "req",
			method: "notAllowlisted",
			payload: {},
		});
		expect(decodeFrameStr(line).method).toBe("notAllowlisted");
		expect(() => decodeFrameStrStrict(line)).toThrow(ProtocolError);
	});

	test("uiSlot accepts scalar and object margins", () => {
		for (const margin of [2, { top: 1, right: 2, bottom: 3, left: 4 }]) {
			const frame = {
				id: 0,
				kind: "event" as const,
				method: "uiSlot",
				payload: {
					key: "slot",
					generation: 1,
					placement: "overlay",
					height: 1,
					runs: [[{ text: "ok" }]],
					overlayOptions: { margin },
				},
			};
			expect(decodeFrameStr(encodeFrameString(frame).trimEnd())).toEqual(frame);
		}
	});

	test("uiSlot rejects forbidden and oversized links", () => {
		const links = [
			{ uri: "javascript:alert(1)" },
			{ uri: "file:///tmp/x" },
			{ uri: `https://example.com/${"x".repeat(2048)}` },
			{ id: "x".repeat(129), uri: "https://example.com" },
		];
		for (const link of links) {
			const frame = {
				id: 0,
				kind: "event" as const,
				method: "uiSlot",
				payload: {
					key: "slot",
					generation: 1,
					placement: "aboveEditor",
					height: 1,
					runs: [[{ text: "bad", style: { link } }]],
				},
			};
			expect(() => encodeFrame(frame)).toThrow(ProtocolError);
			expect(() => decodeFrameStr(JSON.stringify(frame))).toThrow(ProtocolError);
		}
	});
});

describe("FrameDecoder", () => {
	test("fragmentation and multiple frames", () => {
		const f1 = requestFrame(1, "hello", localHello());
		const f2 = {
			id: 1,
			kind: "res" as const,
			method: "hello",
			payload: localHello(),
		};
		const bytes = new Uint8Array([...encodeFrame(f1), ...encodeFrame(f2)]);
		const dec = new FrameDecoder();
		const got = [];
		for (const b of bytes) {
			got.push(...dec.push(new Uint8Array([b])));
		}
		expect(dec.finish()).toBeUndefined();
		expect(got).toEqual([f1, f2]);
	});

	test("CRLF", () => {
		const frame = requestFrame(1, "hello", localHello());
		const json = JSON.stringify(frame);
		const line = new TextEncoder().encode(`${json}\r\n`);
		const dec = new FrameDecoder();
		expect(dec.push(line)).toEqual([frame]);
	});

	test("final line without newline", () => {
		const frame = requestFrame(1, "hello", localHello());
		const line = new TextEncoder().encode(JSON.stringify(frame));
		const dec = new FrameDecoder();
		expect(dec.push(line)).toEqual([]);
		expect(dec.finishWithFinalLine()).toEqual(frame);

		const dec2 = new FrameDecoder();
		dec2.push(line);
		expect(() => dec2.finish()).toThrow(ProtocolError);
	});

	test("invalid utf8 and json", () => {
		const dec = new FrameDecoder();
		expect(() => dec.push(new Uint8Array([0xff, 0x0a]))).toThrow(ProtocolError);

		const dec2 = new FrameDecoder();
		expect(() => dec2.push(new TextEncoder().encode("{not-json}\n"))).toThrow(ProtocolError);
	});

	test("oversized before growth", () => {
		const limit = 64;
		const dec = new FrameDecoder(limit);
		expect(() => dec.push(new Uint8Array(limit + 1).fill(0x61))).toThrow(ProtocolError);
		expect(dec.bufferedLen).toBe(0);

		const dec2 = new FrameDecoder(limit);
		dec2.push(new Uint8Array(limit / 2).fill(0x62));
		expect(() => dec2.push(new Uint8Array(limit).fill(0x63))).toThrow(ProtocolError);
	});
});

describe("shared fixtures", () => {
	test("field and discriminant parity", () => {
		const text = readFileSync(fixturesPath, "utf8");
		let count = 0;
		for (const line of text.split("\n")) {
			if (line.trim() === "" || line.trimStart().startsWith("#")) {
				continue;
			}
			const frame = decodeFrameStr(line);
			const again = decodeFrameStr(encodeFrameString(frame).trimEnd());
			expect(again).toEqual(frame);
			count += 1;
		}
		expect(count).toBeGreaterThanOrEqual(8);
	});

	test("bridge open methods are witnessed with both directions", () => {
		const text = readFileSync(fixturesPath, "utf8");
		const seen = new Set<string>();
		for (const line of text.split("\n")) {
			if (line.trim() === "" || line.trimStart().startsWith("#")) {
				continue;
			}
			const frame = decodeFrameStr(line);
			seen.add(`${frame.method}:${frame.kind}`);
		}
		for (const key of [
			"theme.update:event",
			"theme.set:event",
			"session.update:event",
			"session.command:event",
			"session.setModel:req",
			"session.compact:req",
			"session.compact:res",
			"session.setModel:res",
			"ui.control:event",
			"ui.state:event",
			"session.setupEntries:req",
			"session.setupEntries:res",
			"session.replacementReady:event",
			"session.replacementAbort:event",
		]) {
			expect(seen).toContain(key);
		}
	});

	test("session.command fixtures cover legacy untagged and candidate-tagged actions", () => {
		const text = readFileSync(fixturesPath, "utf8");
		const legacy = new Set<string>();
		const candidate = new Set<string>();
		for (const line of text.split("\n")) {
			if (line.trim() === "" || line.trimStart().startsWith("#")) {
				continue;
			}
			const frame = decodeFrameStr(line);
			if (frame.method !== "session.command" || frame.kind !== "event") {
				continue;
			}
			const payload = frame.payload as Record<string, unknown>;
			const action = payload["action"];
			expect(typeof action).toBe("string");
			const round = decodeFrameStr(encodeFrameString(frame).trimEnd());
			expect(round.payload).toEqual(payload);
			if ("replacementToken" in payload) {
				expect(typeof payload["replacementToken"]).toBe("string");
				candidate.add(action as string);
			} else {
				legacy.add(action as string);
			}
		}
		expect([...legacy].sort()).toEqual(["sendMessage", "setSessionName", "shutdown"]);
		expect([...candidate].sort()).toEqual(["sendMessage", "setSessionName", "shutdown"]);
	});
});

describe("witness manifest lockstep", () => {
	// The single (method, kind) witness-manifest lockstep test (XC-2).
	// Both language sides consume witness-manifest.json by name — parity
	// does not create a second check.  Deleting any fixture line or
	// mutating a modifier-combo key event kind breaks this test.
	const manifestPath = join(
		dirname(fileURLToPath(import.meta.url)),
		"fixtures",
		"witness-manifest.json",
	);
	const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as {
		totalLines: number;
		methodKindPairs: [string, string][];
		modifierComboKeyEvents: {
			code: string;
			modifiers: Record<string, boolean>;
			kind: string;
		}[];
	};

	test("total non-blank line count matches manifest", () => {
		const text = readFileSync(fixturesPath, "utf8");
		let count = 0;
		for (const line of text.split("\n")) {
			if (line.trim() === "" || line.trimStart().startsWith("#")) {
				continue;
			}
			count += 1;
		}
		expect(count).toBe(manifest.totalLines);
	});

	test("every manifest (method, kind) pair is witnessed in frames.jsonl", () => {
		const text = readFileSync(fixturesPath, "utf8");
		const seen = new Set<string>();
		for (const line of text.split("\n")) {
			if (line.trim() === "" || line.trimStart().startsWith("#")) {
				continue;
			}
			const frame = decodeFrameStr(line);
			seen.add(`${frame.method}:${frame.kind}`);
		}
		for (const [method, kind] of manifest.methodKindPairs) {
			expect(seen).toContain(`${method}:${kind}`);
		}
		// Every seen pair must also be in the manifest (no untracked fixtures).
		for (const key of seen) {
			const [method, kind] = key.split(":");
			expect(manifest.methodKindPairs).toContainEqual([method, kind]);
		}
	});

	test("modifier-combo key events match manifest exactly", () => {
		const text = readFileSync(fixturesPath, "utf8");
		const keyEvents: typeof manifest.modifierComboKeyEvents = [];
		for (const line of text.split("\n")) {
			if (line.trim() === "" || line.trimStart().startsWith("#")) {
				continue;
			}
			const frame = decodeFrameStr(line);
			if (frame.method !== "uiEvent" || frame.kind !== "req") {
				continue;
			}
			const payload = frame.payload as Record<string, unknown>;
			const event = payload["event"] as Record<string, unknown>;
			if (event["type"] !== "key") {
				continue;
			}
			keyEvents.push({
				code: event["code"] as string,
				modifiers: (event["modifiers"] ?? {}) as Record<string, boolean>,
				kind: (event["kind"] ?? "press") as string,
			});
		}
		expect(keyEvents).toEqual(manifest.modifierComboKeyEvents);
	});
});
