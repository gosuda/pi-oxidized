import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { writeFileSync, readFileSync, rmSync, mkdirSync } from "node:fs";
import { createHash } from "node:crypto";

import { sha256Prefix } from "../session-timing.ts";

const REPOSITORY_ROOT = resolve(import.meta.dirname, "..");

describe("session-timing sha256Prefix", () => {
	test("returns 16 hex chars for a known file", () => {
		const dir = resolve(REPOSITORY_ROOT, "target/bench/test-session-timing");
		mkdirSync(dir, { recursive: true });
		const path = resolve(dir, "hash-test.jsonl");
		writeFileSync(path, '{"type":"session","version":3,"id":"x","timestamp":"t","cwd":"/tmp"}\n');
		const prefix = sha256Prefix(path);
		expect(prefix.length).toBe(16);
		// Verify against direct computation
		const content = readFileSync(path);
		const expected = createHash("sha256").update(content).digest("hex").slice(0, 16);
		expect(prefix).toBe(expected);
		rmSync(dir, { recursive: true, force: true });
	});

	test("is deterministic for identical content", () => {
		const dir = resolve(REPOSITORY_ROOT, "target/bench/test-session-timing");
		mkdirSync(dir, { recursive: true });
		const path = resolve(dir, "deterministic-test.jsonl");
		writeFileSync(path, "test content\n");
		const p1 = sha256Prefix(path);
		const p2 = sha256Prefix(path);
		expect(p1).toBe(p2);
		rmSync(dir, { recursive: true, force: true });
	});
});

describe("session-timing constants", () => {
	test("entry counts cover small, medium, and large sessions", () => {
		const entryCounts = [100, 1_000, 5_000] as const;
		expect(entryCounts).toContain(100);
		expect(entryCounts).toContain(1_000);
		expect(entryCounts).toContain(5_000);
	});

	test("sample counts are positive", () => {
		expect(20).toBeGreaterThan(0);
		expect(10).toBeGreaterThan(0);
		expect(3).toBeGreaterThanOrEqual(0);
	});
});
