/**
 * Tests for the mode-2 distinctness gate: pure statistics/threshold units,
 * plus one low-count subprocess smoke that drives real compat and lean host
 * children through hello -> extensions.load and the lean 3-RPC round-trip.
 */
import { describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { evaluateZeroIdleSanity } from "../bench-extension-scaling.ts";
import {
	ChildHost,
	DEFAULT_HOSTILE_OUTPUT_CEILING,
	DEFAULT_LEAN_MAX_RATIO,
	MAX_RETAINED_FRAMES,
	MAX_STDOUT_BUFFER_CHARS,
	TimerHandle,
	deriveRetainedFrameBudget,
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

describe("retained frame budgets", () => {
	test("scales configured tool rounds without exceeding the hostile ceiling", () => {
		expect(MAX_RETAINED_FRAMES).toBe(128);
		expect(DEFAULT_HOSTILE_OUTPUT_CEILING).toBe(10_000);
		expect(deriveRetainedFrameBudget(2)).toBe(128);
		expect(deriveRetainedFrameBudget(50)).toBe(216);
		expect(deriveRetainedFrameBudget(3_000)).toBe(10_000);
	});
});

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

describe("zero/idle extension scaling sanity", () => {
	test("flags injected keypress and frame regressions independently", () => {
		expect(
			evaluateZeroIdleSanity(
				{ keypressP99: 1, frameP99: 1 },
				{ keypressP99: 1.5, frameP99: 1 },
			),
		).toEqual(["idle keypress p99 1.500ms > 110% of zero 1.000ms"]);
		expect(
			evaluateZeroIdleSanity(
				{ keypressP99: 1, frameP99: 1 },
				{ keypressP99: 1, frameP99: 1.5 },
			),
		).toEqual(["idle frame p99 1.500ms > 110% of zero 1.000ms"]);
	});

	test("permits idle metrics at the zero-extension threshold", () => {
		expect(
			evaluateZeroIdleSanity(
				{ keypressP99: 10, frameP99: 10 },
				{ keypressP99: 11, frameP99: 11 },
			),
		).toEqual([]);
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

describe("ChildHost request write failure", () => {
	test("removes the response waiter before rejecting and leaves no unhandled rejection", async () => {
		const root = resolve(process.cwd(), "packages", "extension-host");
		const tempDir = mkdtempSync(join(tmpdir(), "lean-scaling-write-failure-"));
		const scriptPath = join(tempDir, "write-failure-host.mjs");
		writeFileSync(
			scriptPath,
			[
				"await Bun.sleep(100);",
				`process.stdout.write(${JSON.stringify("not-json\n")});`,
				"setInterval(() => {}, 1_000);",
			].join("\n"),
			"utf8",
		);

		const unhandled: unknown[] = [];
		const trackUnhandled = (err: unknown) => {
			unhandled.push(err);
		};
		process.on("unhandledRejection", trackUnhandled);

		const host = new ChildHost({
			hostCwd: root,
			hostEntry: scriptPath,
			lean: false,
			extensionPath: resolve(root, "tests", "fixtures", "lean", "echo.mjs"),
		});
		// Deliberate error-path probe: install a synchronous stdin write failure
		// without changing the public harness contract.
		const child = Reflect.get(host, "child");
		if (typeof child !== "object" || child === null) {
			throw new Error("ChildHost child is unavailable");
		}
		const stdin = Reflect.get(child, "stdin");
		if (typeof stdin !== "object" || stdin === null) {
			throw new Error("ChildHost stdin is unavailable");
		}
		const originalWrite = Reflect.get(stdin, "write");
		if (typeof originalWrite !== "function") {
			throw new Error("ChildHost stdin.write is unavailable");
		}
		if (
			!Reflect.set(stdin, "write", () => {
				throw new Error("synthetic stdin write failure");
			})
		) {
			throw new Error("failed to replace ChildHost stdin.write");
		}

		const pendingWaiters = (): unknown[] => {
			const waiters = Reflect.get(host, "waiters");
			if (!Array.isArray(waiters)) throw new Error("ChildHost waiters are unavailable");
			return waiters;
		};

		try {
			await expect(host.request("hello", {}, 60_000)).rejects.toThrow(
				/failed to write hello: synthetic stdin write failure/,
			);
			expect(pendingWaiters()).toHaveLength(0);
			// The later malformed frame exercises failAll. A stale response promise
			// would reject here after request() had already returned.
			const laterFailure = await host
				.waitFor(() => false, 60_000, "malformed child output")
				.catch((error: unknown) => error);
			if (!(laterFailure instanceof Error)) {
				throw new Error("malformed child output did not reject with an Error");
			}
			expect(laterFailure.message).toMatch(/failed to parse host stdout JSON/);
			await new Promise<void>((resolveImmediate) => setImmediate(resolveImmediate));
			expect(pendingWaiters()).toHaveLength(0);
			expect(unhandled).toEqual([]);
		} finally {
			Reflect.set(stdin, "write", originalWrite);
			process.off("unhandledRejection", trackUnhandled);
			await host.close();
			rmSync(tempDir, { recursive: true, force: true });
		}
	}, 30_000);
});

describe("ChildHost malformed stdout", () => {
	test("rejects every pending waiter promptly on malformed NDJSON", async () => {
		const root = resolve(process.cwd(), "packages", "extension-host");
		const tempDir = mkdtempSync(join(tmpdir(), "lean-scaling-malformed-"));
		const scriptPath = join(tempDir, "malformed-host.mjs");
		const sentinelPath = join(tempDir, "child-survived");
		const hostileStdout = `{"not":"json"\x00${"A".repeat(8_000)}`;
		// Marker at the end so the 4096-byte stderrTail still carries identifiable context.
		const hostileStderr = `${"B".repeat(5_000)}stderr-context-tail`;
		const postFailureFrame = `${JSON.stringify({
			id: 99,
			kind: "event",
			method: "toolUpdate",
			payload: { afterMalformed: true },
		})}\n`;
		writeFileSync(
			scriptPath,
			[
				"await Bun.sleep(150);",
				`process.stderr.write(${JSON.stringify(hostileStderr)});`,
				`process.stdout.write(${JSON.stringify(`${hostileStdout}\n`)});`,
				"await Bun.sleep(50);",
				`process.stdout.write(${JSON.stringify(postFailureFrame)});`,
				"await Bun.sleep(500);",
				`await Bun.write(${JSON.stringify(sentinelPath)}, "child-survived");`,
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
			// DIAGNOSTIC_STDOUT_PREFIX_CHARS (512) + ellipsis marker "…" — pin from below
			// so lowering the cap fails (upper-bound-only would still pass at 64).
			expect(stdoutSnippet.length).toBe(513);
			expect(stdoutSnippet.endsWith("…")).toBe(true);
			expect(stdoutSnippet.startsWith(hostileStdout.slice(0, 32))).toBe(true);
			// stderrTail is capped at 4096 before diagnosticSnippet(..., 4096); fixture
			// exceeds that, so the decoded snippet must be exactly the cap (no ellipsis
			// when length === maxChars). Pin from below so lowering the cap fails.
			expect(stderrSnippet.length).toBe(4096);
			expect(stderrSnippet.endsWith("stderr-context-tail")).toBe(true);
			expect(unhandled).toEqual([]);
			// The fatal parse path must kill the hostile child rather than merely
			// rejecting its current waiters.
			// This is a real child-process reaping proof; fake timers cannot advance
			// the OS process that must be killed before its sentinel write.
			await Bun.sleep(750);
			expect(existsSync(sentinelPath)).toBe(false);
			expect(host.frames).toEqual([]);
		} finally {
			process.off("uncaughtException", trackUnhandled);
			process.off("unhandledRejection", trackUnhandled);
			await host.close();
			rmSync(tempDir, { recursive: true, force: true });
		}
	}, 30_000);
});

describe("runModeDistinctness toolRounds", () => {
	test("rejects zero/negative/non-integer round counts before collecting samples", async () => {
		const hostCwd = resolve(process.cwd(), "packages", "extension-host");
		const base = {
			hostCwd,
			hostEntry: "src/main.ts",
			compatExtension: resolve(hostCwd, "fixtures", "extensions", "idle.ts"),
			leanExtension: resolve(hostCwd, "tests", "fixtures", "lean", "echo.mjs"),
			warmups: 0,
			samples: 0,
			maxRatio: undefined,
		} as const;

		for (const toolRounds of [0, -1, 1.5, Number.NaN]) {
			await expect(runModeDistinctness({ ...base, toolRounds })).rejects.toThrow(
				/toolRounds must be a positive integer/,
			);
		}
		// Fallback path: omitted toolRounds uses samples, which must also be positive.
		await expect(runModeDistinctness({ ...base })).rejects.toThrow(
			/toolRounds must be a positive integer/,
		);
	});
});

	test("allows configured tool rounds above the legacy retained-frame cap", async () => {
		const hostCwd = resolve(process.cwd(), "packages", "extension-host");
		const result = await runModeDistinctness({
			hostCwd,
			hostEntry: "src/main.ts",
			compatExtension: resolve(hostCwd, "fixtures", "extensions", "idle.ts"),
			leanExtension: resolve(hostCwd, "tests", "fixtures", "lean", "echo.mjs"),
			warmups: 0,
			samples: 0,
			toolRounds: 32,
			maxRatio: undefined,
		});

		expect(result.toolRoundTrip.rounds).toBe(32);
		expect(result.toolRoundTrip.responses).toBe(96);
		expect(result.toolRoundTrip.updateEvents).toBeGreaterThanOrEqual(32);
		expect(result.toolRoundTrip.prepareMs.n).toBe(32);
		expect(result.toolRoundTrip.validateMs.n).toBe(32);
		expect(result.toolRoundTrip.executeMs.n).toBe(32);
		expect(result.failures).toEqual([]);
	}, 180_000);

describe("runModeDistinctness validatedBy gate", () => {
	test("rejects a lean fixture whose validate merely echoes args (no validatedBy marker)", async () => {
		const hostCwd = resolve(process.cwd(), "packages", "extension-host");
		const tempDir = mkdtempSync(join(tmpdir(), "lean-scaling-validatedby-"));
		const leanPath = join(tempDir, "echo-no-validated-by.mjs");
		// Mirrors the shared lean echo fixture but validate returns args verbatim
		// without stamping validatedBy, so the gate must reject it and name validate.
		writeFileSync(
			leanPath,
			[
				"export default {",
				"  name: \"lean-echo-no-validate\",",
				"  tools: [",
				"    {",
				"      name: \"echo\",",
				"      description: \"Echo without a validate marker\",",
				"      parameters: { type: \"object\", properties: { text: { type: \"string\" } }, required: [\"text\"] },",
				"      prepare: (args) => ({ ...args, preparedBy: \"lean\" }),",
				"      validate: (args) => args,",
				"      execute: (args, ctx) => {",
				"        ctx.onUpdate({ content: [{ type: \"text\", text: \"echoing...\" }] });",
				"        return { content: [{ type: \"text\", text: `echo:${args.text}` }] };",
				"      },",
				"    },",
				"  ],",
				"};",
			].join("\n"),
			"utf8",
		);

		try {
			const result = runModeDistinctness({
				hostCwd,
				hostEntry: "src/main.ts",
				compatExtension: resolve(hostCwd, "fixtures", "extensions", "idle.ts"),
				leanExtension: leanPath,
				warmups: 0,
				samples: 1,
				toolRounds: 1,
				maxRatio: undefined,
			});
			// Capture the actual rejection so we assert the gate — not a
			// load-time failure that happens to mention validate — rejected it.
			const rejection = await result.catch((err: unknown) => err);
			expect(rejection).toBeInstanceOf(Error);
			expect((rejection as Error).message).toMatch(
				/^tool\.validate did not perform real work:/,
			);
		} finally {
			rmSync(tempDir, { recursive: true, force: true });
		}
	}, 60_000);
});

describe("ChildHost UTF-8 framing", () => {
	test("reassembles a multibyte character split across stdout chunks", async () => {
		const root = resolve(process.cwd(), "packages", "extension-host");
		const tempDir = mkdtempSync(join(tmpdir(), "lean-scaling-utf8-"));
		const scriptPath = join(tempDir, "utf8-split-host.mjs");

		// Payload contains U+1F680 (🚀, 4-byte UTF-8 F0 9F 9A 80). The host
		// writes the JSON line as two buffers that split mid-character so a
		// naive chunk.toString() would inject U+FFFD and corrupt the frame.
		const marker = "before-\u{1F680}-after";
		const frame = {
			id: 1,
			kind: "res",
			method: "hello",
			payload: { marker },
		};
		const lineBytes = Buffer.from(`${JSON.stringify(frame)}\n`, "utf8");
		const rocket = Buffer.from("\u{1F680}", "utf8");
		const splitAt = lineBytes.indexOf(rocket) + 2; // mid 4-byte sequence
		if (splitAt < 2 || splitAt >= lineBytes.length) {
			throw new Error(`failed to locate mid-rocket split point (splitAt=${splitAt})`);
		}

		writeFileSync(
			scriptPath,
			[
				`const line = Buffer.from(${JSON.stringify(lineBytes.toString("base64"))}, "base64");`,
				`const splitAt = ${splitAt};`,
				"await Bun.sleep(50);",
				"process.stdout.write(line.subarray(0, splitAt));",
				"await Bun.sleep(50);",
				"process.stdout.write(line.subarray(splitAt));",
				"setInterval(() => {}, 1_000);",
			].join("\n"),
			"utf8",
		);

		const host = new ChildHost({
			hostCwd: root,
			hostEntry: scriptPath,
			lean: false,
			extensionPath: resolve(root, "tests", "fixtures", "lean", "echo.mjs"),
		});
		try {
			const matched = await host.waitFor(
				(f) => f.kind === "res" && f.method === "hello" && f.id === 1,
				10_000,
				"utf8 hello frame",
			);
			const payload = matched.payload as { marker?: string };
			expect(payload.marker).toBe(marker);
			expect(payload.marker).toContain("\u{1F680}");
			expect(payload.marker).not.toContain("\uFFFD");
		} finally {
			await host.close();
			rmSync(tempDir, { recursive: true, force: true });
		}
	}, 30_000);
});

describe("ChildHost stdout buffer cap", () => {
	test("failAll+reaps when an unterminated line exceeds the buffer cap", async () => {
		const root = resolve(process.cwd(), "packages", "extension-host");
		const tempDir = mkdtempSync(join(tmpdir(), "lean-scaling-overflow-"));
		const scriptPath = join(tempDir, "overflow-host.mjs");
		const overflowChars = MAX_STDOUT_BUFFER_CHARS + 1024;
		writeFileSync(
			scriptPath,
			[
				"await Bun.sleep(50);",
				`process.stdout.write("X".repeat(${overflowChars}));`,
				"setInterval(() => {}, 1_000);",
			].join("\n"),
			"utf8",
		);

		const host = new ChildHost({
			hostCwd: root,
			hostEntry: scriptPath,
			lean: false,
			extensionPath: resolve(root, "tests", "fixtures", "lean", "echo.mjs"),
		});
		try {
			const pending = host.waitFor(() => false, 60_000, "never-arriving frame");
			const started = performance.now();
			const rejection = await pending.catch((err: unknown) => err);
			const elapsedMs = performance.now() - started;

			expect(elapsedMs).toBeLessThan(5_000);
			expect(rejection).toBeInstanceOf(Error);
			const message = (rejection as Error).message;
			expect(message).toMatch(/unterminated line exceeded/);
			expect(message).toContain(String(MAX_STDOUT_BUFFER_CHARS));
			expect(message).toContain("stdout:");
			expect(message).toContain("stderr:");
			const stdoutEncoded = /stdout: ("(?:[^"\\]|\\.)*")/.exec(message)?.[1];
			expect(stdoutEncoded).toBeDefined();
			const stdoutSnippet = JSON.parse(stdoutEncoded!) as string;
			// diagnosticSnippet caps at DIAGNOSTIC_STDOUT_PREFIX_CHARS (512) + "…".
			expect(stdoutSnippet.length).toBe(513);
			expect(stdoutSnippet.endsWith("…")).toBe(true);
			expect(stdoutSnippet.startsWith("XXXX")).toBe(true);
			// Reap path: close() should finish promptly because SIGKILL already fired.
			const closeStarted = performance.now();
			await host.close(100);
			expect(performance.now() - closeStarted).toBeLessThan(2_000);
		} finally {
			await host.close();
			rmSync(tempDir, { recursive: true, force: true });
		}
	}, 30_000);
});

describe("ChildHost retained frame cap", () => {
	test("fails every waiter and reaps after a valid-frame flood reaches the cap", async () => {
		const root = resolve(process.cwd(), "packages", "extension-host");
		const tempDir = mkdtempSync(join(tmpdir(), "lean-scaling-frame-flood-"));
		const scriptPath = join(tempDir, "frame-flood-host.mjs");
		const sentinelPath = join(tempDir, "child-survived");
		const retainedFrameLimit = 3;
		const frame = { id: 1, kind: "event", method: "toolUpdate", payload: { ok: true } };
		const flood = `${JSON.stringify(frame)}\n`.repeat(retainedFrameLimit + 1);
		writeFileSync(
			scriptPath,
			[
				"await Bun.sleep(50);",
				`process.stdout.write(${JSON.stringify(flood)});`,
				"await Bun.sleep(500);",
				`await Bun.write(${JSON.stringify(sentinelPath)}, "child-survived");`,
				"setInterval(() => {}, 1_000);",
			].join("\n"),
			"utf8",
		);

		const host = new ChildHost({
			hostCwd: root,
			hostEntry: scriptPath,
			lean: false,
			extensionPath: resolve(root, "tests", "fixtures", "lean", "echo.mjs"),
			maxRetainedFrames: retainedFrameLimit,
		});
		try {
			const waiters = [
				host.waitFor(() => false, 1_500, "waiter-a"),
				host.waitFor(() => false, 1_500, "waiter-b"),
				host.waitFor(() => false, 1_500, "waiter-c"),
			];
			const started = performance.now();
			const rejections = await Promise.all(waiters.map((pending) => pending.catch((err: unknown) => err)));

			expect(performance.now() - started).toBeLessThan(1_000);
			const messages = rejections.map((err) => {
				expect(err).toBeInstanceOf(Error);
				return (err as Error).message;
			});
			expect(new Set(messages).size).toBe(1);
			expect(messages[0]).toMatch(/retained frame limit exceeded/);
			await expect(host.waitFor(() => false, 60_000, "post-failure waiter")).rejects.toThrow(
				/retained frame limit exceeded/,
			);
			expect(host.frames.length).toBeLessThanOrEqual(retainedFrameLimit);
			// This is a real child-process reaping proof; fake timers cannot advance
			// the OS process that must be killed before its sentinel write.
			await Bun.sleep(750);
			expect(existsSync(sentinelPath)).toBe(false);
		} finally {
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
