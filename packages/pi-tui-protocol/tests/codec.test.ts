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
	localHello,
	MAX_FRAME_BYTES,
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
});
