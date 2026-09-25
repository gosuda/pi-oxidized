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
});

