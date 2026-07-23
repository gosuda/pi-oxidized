/**
 * Tests for the mode-2 distinctness gate: pure statistics/threshold units,
 * plus one low-count subprocess smoke that drives real compat and lean host
 * children through hello -> extensions.load and the lean 3-RPC round-trip.
 */
import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import {
	ChildHost,
	DEFAULT_LEAN_MAX_RATIO,
	TimerHandle,
	evaluateDistinctness,
	percentile,
	runModeDistinctness,
	summarize,
} from "../lean-scaling.ts";

/** Track timer handles created/cleared while this helper is active. */
function interceptTimers() {
	const handles = new Set<TimerHandle>();
	const originalSetTimeout = globalThis.setTimeout;
	const originalClearTimeout = globalThis.clearTimeout;

	globalThis.setTimeout = ((...args: Parameters<typeof originalSetTimeout>) => {
		const handle = originalSetTimeout(...args);
		handles.add(handle);
		return handle;
	}) as typeof originalSetTimeout;

	globalThis.clearTimeout = ((handle: TimerHandle) => {
		handles.delete(handle);
		return originalClearTimeout(handle);
	}) as typeof originalClearTimeout;

	return {
		handles,
		restore() {
			globalThis.setTimeout = originalSetTimeout;
			globalThis.clearTimeout = originalClearTimeout;
		},
	};
}

describe("percentile", () => {
	test("empty input yields zero", () => {
		expect(percentile([], 50)).toBe(0);
		expect(percentile([], 95)).toBe(0);
	});

	test("single element is every percentile", () => {
		expect(percentile([7], 50)).toBe(7);
		expect(percentile([7], 95)).toBe(7);
	});

	test("picks the ceil-rank element of the sorted input", () => {
		const sorted = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
		expect(percentile(sorted, 50)).toBe(5);
		expect(percentile(sorted, 95)).toBe(10);
		expect(percentile(sorted, 0)).toBe(1);
		expect(percentile(sorted, 100)).toBe(10);
	});
});

describe("summarize", () => {
	test("empty input yields zeros with n=0", () => {
		expect(summarize([])).toEqual({ n: 0, min: 0, median: 0, p95: 0, max: 0, mean: 0 });
	});

	test("computes median/p95/min/max/mean over unsorted input", () => {
		const summary = summarize([9, 1, 5, 3, 7]);
		expect(summary.n).toBe(5);
		expect(summary.min).toBe(1);
		expect(summary.max).toBe(9);
		expect(summary.median).toBe(5);
		expect(summary.mean).toBe(5);
		expect(summary.p95).toBe(9);
	});

	test("does not mutate the input array", () => {
		const input = [3, 1, 2];
		summarize(input);
		expect(input).toEqual([3, 1, 2]);
	});
});

describe("evaluateDistinctness", () => {
	test("ratio below the max passes", () => {
		const verdict = evaluateDistinctness(100, 80, DEFAULT_LEAN_MAX_RATIO);
		expect(verdict.ratio).toBeCloseTo(0.8);
		expect(verdict.pass).toBe(true);
		expect(verdict.enforced).toBe(true);
	});

	test("ratio exactly at the max passes (inclusive bound)", () => {
		expect(evaluateDistinctness(100, 85, DEFAULT_LEAN_MAX_RATIO).pass).toBe(true);
	});

	test("ratio above the max fails", () => {
		const verdict = evaluateDistinctness(100, 90, DEFAULT_LEAN_MAX_RATIO);
		expect(verdict.ratio).toBeCloseTo(0.9);
		expect(verdict.pass).toBe(false);
	});

	test("lean at parity with compat fails any sub-1.0 gate", () => {
		expect(evaluateDistinctness(50, 50, DEFAULT_LEAN_MAX_RATIO).pass).toBe(false);
	});

	test("non-positive compat median cannot pass an enforced gate", () => {
		const verdict = evaluateDistinctness(0, 0, DEFAULT_LEAN_MAX_RATIO);
		expect(verdict.ratio).toBe(Number.POSITIVE_INFINITY);
		expect(verdict.pass).toBe(false);
	});

	test("omitted max ratio measures without enforcing", () => {
		const verdict = evaluateDistinctness(100, 150, undefined);
		expect(verdict.ratio).toBeCloseTo(1.5);
		expect(verdict.pass).toBe(true);
		expect(verdict.enforced).toBe(false);
	});
});

describe("ChildHost.close", () => {
	test("clears graceful-exit and SIGKILL race timers when the child exits", async () => {
		const hostCwd = resolve(process.cwd(), "packages", "extension-host");
		const host = new ChildHost({
			hostCwd,
			hostEntry: "src/main.ts",
			lean: true,
			extensionPath: resolve(hostCwd, "tests", "fixtures", "lean", "echo.mjs"),
		});
		// Keep a waiter pending so the child is still alive when close() runs.
		const pending = host.waitFor(() => false, 60_000, "never-arriving frame");

		const { handles, restore } = interceptTimers();
		try {
			const [rejection] = await Promise.all([
				pending.catch((err: unknown) => err),
				// Force the SIGKILL race path so both timers are created.
				host.close(0),
			]);
			expect(rejection).toBeInstanceOf(Error);
			expect((rejection as Error).message).toMatch(/closed|exited/);
			expect(handles.size).toBe(0);
		} finally {
			restore();
		}
	}, 30_000);
});

describe("ChildHost malformed stdout", () => {
	test("rejects every pending waiter promptly on malformed NDJSON", async () => {
		const { mkdtempSync, rmSync, writeFileSync } = await import("node:fs");
		const { tmpdir } = await import("node:os");
		const { join } = await import("node:path");
		const root = resolve(process.cwd(), "packages", "extension-host");
		const tempDir = mkdtempSync(join(tmpdir(), "lean-scaling-malformed-"));
		const scriptPath = join(tempDir, "malformed-host.mjs");
		const hostileStdout = `{"not":"json"\x00${"A".repeat(8_000)}`;
		// Marker at the end so the 4096-byte stderrTail still carries identifiable context.
		const hostileStderr = `${"B".repeat(5_000)}stderr-context-tail`;
		writeFileSync(
			scriptPath,
			[
				"await Bun.sleep(150);",
				`process.stderr.write(${JSON.stringify(hostileStderr)});`,
				`process.stdout.write(${JSON.stringify(`${hostileStdout}\n`)});`,
				"setInterval(() => {}, 1_000);",
			].join("\n"),
			"utf8",
		);

		const unhandled: unknown[] = [];
		const trackUnhandled = (err: unknown) => {
			unhandled.push(err);
		};
		process.on("uncaughtException", trackUnhandled);
		process.on("unhandledRejection", trackUnhandled);

		const host = new ChildHost({
			hostCwd: root,
			hostEntry: scriptPath,
			lean: false,
			extensionPath: resolve(root, "tests", "fixtures", "lean", "echo.mjs"),
		});
		try {
			const waiters = [
				host.waitFor(() => false, 60_000, "waiter-a"),
				host.waitFor(() => false, 60_000, "waiter-b"),
				host.waitFor(() => false, 60_000, "waiter-c"),
			];
			const started = performance.now();
			const rejections = await Promise.all(waiters.map((pending) => pending.catch((err: unknown) => err)));
			const elapsedMs = performance.now() - started;

			expect(elapsedMs).toBeLessThan(5_000);
			expect(rejections).toHaveLength(3);
			const messages = rejections.map((err) => {
				expect(err).toBeInstanceOf(Error);
				return (err as Error).message;
			});
			expect(new Set(messages).size).toBe(1);
			const message = messages[0] ?? "";
			expect(message).toMatch(/failed to parse host stdout JSON/);
			expect(message).toContain("stdout:");
			expect(message).toContain("stderr:");
			// Prefixes are present but bounded/escaped (JSON string encoding + truncation).
			expect(message).toContain("\\u0000");
			expect(message).toContain("stderr-context-tail");
			// Extract JSON-encoded diagnostic snippets (diagnosticSnippet in lean-scaling.ts).
			const stdoutEncoded = /stdout: ("(?:[^"\\]|\\.)*")/.exec(message)?.[1];
			const stderrEncoded = /stderr: ("(?:[^"\\]|\\.)*")/.exec(message)?.[1];
			expect(stdoutEncoded).toBeDefined();
			expect(stderrEncoded).toBeDefined();
			const stdoutSnippet = JSON.parse(stdoutEncoded!) as string;
			const stderrSnippet = JSON.parse(stderrEncoded!) as string;
			// DIAGNOSTIC_STDOUT_PREFIX_CHARS (512) + ellipsis marker "…"
			expect(stdoutSnippet.length).toBeLessThanOrEqual(513);
			expect(stdoutSnippet.endsWith("…")).toBe(true);
			expect(stdoutSnippet.startsWith(hostileStdout.slice(0, 32))).toBe(true);
			// stderrTail is capped at 4096 before diagnosticSnippet(..., 4096).
			expect(stderrSnippet.length).toBeLessThanOrEqual(4096);
			expect(stderrSnippet.endsWith("stderr-context-tail")).toBe(true);
			expect(unhandled).toEqual([]);
		} finally {
			process.off("uncaughtException", trackUnhandled);
			process.off("unhandledRejection", trackUnhandled);
			await host.close();
			rmSync(tempDir, { recursive: true, force: true });
		}
	}, 30_000);
});

describe("mode distinctness smoke", () => {
	test(
		"drives real compat and lean children plus the lean 3-RPC proof",
		async () => {
			const hostCwd = resolve(process.cwd(), "packages", "extension-host");
			const result = await runModeDistinctness({
				hostCwd,
				hostEntry: "src/main.ts",
				compatExtension: resolve(hostCwd, "fixtures", "extensions", "idle.ts"),
				leanExtension: resolve(hostCwd, "tests", "fixtures", "lean", "echo.mjs"),
				warmups: 1,
				samples: 2,
				toolRounds: 2,
				// No ratio gate here: two samples cannot support a stable ratio.
				maxRatio: undefined,
			});

			// Both modes produced the requested number of post-warmup samples,
			// each internally consistent (hello <= load terminal <= total).
			expect(result.compat.samples).toBe(2);
			expect(result.lean.samples).toBe(2);
			for (const stats of [result.compat, result.lean]) {
				expect(stats.helloMs.n).toBe(2);
				expect(stats.loadMs.n).toBe(2);
				expect(stats.totalMs.n).toBe(2);
				expect(stats.totalMs.min).toBeGreaterThan(0);
				expect(stats.totalMs.min).toBeGreaterThanOrEqual(stats.loadMs.min);
			}
			expect(Number.isFinite(result.verdict.ratio)).toBe(true);
			expect(result.verdict.enforced).toBe(false);

			// Per-call contract: all three RPC stages answered for every round
			// and the execute stage streamed an update per round.
			expect(result.toolRoundTrip.rounds).toBe(2);
			expect(result.toolRoundTrip.responses).toBe(6);
			expect(result.toolRoundTrip.updateEvents).toBeGreaterThanOrEqual(2);
			expect(result.toolRoundTrip.prepareMs.n).toBe(2);
			expect(result.toolRoundTrip.validateMs.n).toBe(2);
			expect(result.toolRoundTrip.executeMs.n).toBe(2);

			expect(result.failures).toEqual([]);
		},
		180_000,
	);

	test("reap: closed children reject pending waiters instead of hanging", async () => {
		const hostCwd = resolve(process.cwd(), "packages", "extension-host");
		const host = new ChildHost({
			hostCwd,
			hostEntry: "src/main.ts",
			lean: true,
			extensionPath: resolve(hostCwd, "tests", "fixtures", "lean", "echo.mjs"),
		});
		const pending = host.waitFor(() => false, 60_000, "never-arriving frame");
		const [rejection] = await Promise.all([
			pending.catch((err: unknown) => err),
			host.close(),
		]);
		expect(rejection).toBeInstanceOf(Error);
		expect((rejection as Error).message).toMatch(/closed|exited/);
	}, 30_000);
});
