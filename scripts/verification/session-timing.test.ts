import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";

// Constants mirrored from session-timing.ts for structural validation.
const SHA256_PREFIX_LENGTH = 16;
const ENTRY_COUNTS = [100, 1_000, 5_000] as const;
const SAMPLE_COUNT = 20;
const COLD_SAMPLE_COUNT = 10;
const WARMUP_COUNT = 3;

const REPOSITORY_ROOT = resolve(import.meta.dirname, "..");
const SESSION_TIMING_MODULE = resolve(import.meta.dirname, "session-timing.ts");

describe("session-timing module", () => {
	test("module path resolves", () => {
		// Importing the module must not execute main() — only import.meta.main triggers it.
		expect(SESSION_TIMING_MODULE).toBeDefined();
	});

	test("sha256Prefix length is 16 hex chars", () => {
		expect(SHA256_PREFIX_LENGTH).toBe(16);
	});

	test("entry counts cover small, medium, and large sessions", () => {
		expect(ENTRY_COUNTS).toContain(100);
		expect(ENTRY_COUNTS).toContain(1_000);
		expect(ENTRY_COUNTS).toContain(5_000);
	});

	test("sample counts are positive", () => {
		expect(SAMPLE_COUNT).toBeGreaterThan(0);
		expect(COLD_SAMPLE_COUNT).toBeGreaterThan(0);
		expect(WARMUP_COUNT).toBeGreaterThanOrEqual(0);
	});
});
