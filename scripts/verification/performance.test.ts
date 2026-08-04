import { afterAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnPty } from "./pty.ts";
import { HarnessFailure, frameObservation, terminateAndRequireCleanExit } from "./performance.ts";

// T33: after capturing the first frame, the performance verifier must send
// /quit and require a clean process exit. The finally force-kill remains
// cleanup only, never a success path. These tests exercise the same
// contract runFirstFrameSample now enforces, using synthetic children that
// emit a synchronized-output frame and then either honor or ignore /quit.

const SYNC_BEGIN = "\x1b[?2026h";
const SYNC_END = "\x1b[?2026l";
const isWindows = process.platform === "win32";
const bunExecutable = process.execPath;

const REPOSITORY_ROOT = resolve(import.meta.dirname, "../..");
const PERFORMANCE_MODULE = resolve(import.meta.dirname, "performance.ts");
const PERFORMANCE_ARTIFACT = resolve(REPOSITORY_ROOT, "target/bench/performance-comparison.json");

const temporaryPaths: string[] = [];

afterAll(() => {
	for (const path of temporaryPaths) rmSync(path, { recursive: true, force: true });
});

function temporaryDirectory(prefix: string): string {
	const path = mkdtempSync(join(tmpdir(), prefix));
	temporaryPaths.push(path);
	return path;
}

test("does not run the benchmark when performance verification is imported", () => {
	const sandbox = temporaryDirectory("perf-import-");
	const artifactBefore = existsSync(PERFORMANCE_ARTIFACT)
		? readFileSync(PERFORMANCE_ARTIFACT)
		: undefined;
	try {
		const imported = Bun.spawnSync(
			[
				bunExecutable,
				"-e",
				[
					"const exitCodeBeforeImport = process.exitCode;",
					`await import(${JSON.stringify(PERFORMANCE_MODULE)});`,
					"if (process.exitCode !== exitCodeBeforeImport) throw new Error('performance import changed process.exitCode');",
				].join("\n"),
			],
			{
				cwd: sandbox,
				env: { ...process.env, TMPDIR: sandbox },
				stdout: "pipe",
				stderr: "pipe",
				timeout: 10_000,
			},
		);
		expect(imported.exitCode).toBe(0);
		expect(new TextDecoder().decode(imported.stdout)).toBe("");
		expect(new TextDecoder().decode(imported.stderr)).toBe("");
		expect(readdirSync(sandbox).filter((entry) => entry.startsWith("pi-check9-"))).toEqual([]);
		const artifactExists = existsSync(PERFORMANCE_ARTIFACT);
		expect(artifactExists).toBe(artifactBefore !== undefined);
		if (artifactBefore !== undefined && artifactExists) {
			expect(Buffer.compare(readFileSync(PERFORMANCE_ARTIFACT), artifactBefore)).toBe(0);
		}
	} finally {
		const artifactExists = existsSync(PERFORMANCE_ARTIFACT);
		const artifactChanged =
			artifactExists !== (artifactBefore !== undefined) ||
			(artifactBefore !== undefined &&
				(!artifactExists || Buffer.compare(readFileSync(PERFORMANCE_ARTIFACT), artifactBefore) !== 0));
		if (artifactChanged) {
			if (artifactBefore !== undefined) writeFileSync(PERFORMANCE_ARTIFACT, artifactBefore);
			else rmSync(PERFORMANCE_ARTIFACT, { force: true });
		}
	}
}, 15_000);

const CLEAN_QUIT_CHILD = `
process.stdout.write(${JSON.stringify(SYNC_BEGIN)} + "frame" + ${JSON.stringify(SYNC_END)} + "\\n");
process.stdin.setEncoding("utf8");
process.stdin.resume();
const iterator = process.stdin[Symbol.asyncIterator]();
await iterator.next();
process.stdin.pause();
process.stdin.destroy();
process.exit(0);
`;

const IGNORE_QUIT_CHILD = `
process.stdout.write(${JSON.stringify(SYNC_BEGIN)} + "frame" + ${JSON.stringify(SYNC_END)} + "\\n");
process.stdin.setEncoding("utf8");
process.stdin.resume();
setInterval(() => {}, 1_000);
`;

const CLEAN_EXIT_CHILD = "process.exit(0);";
const FAILURE_EXIT_CHILD = "process.exit(7);";

describe.skipIf(isWindows)("performance first-frame lifecycle", () => {
	// Internal deadlines exercised by the ignore-quit test: 5_000ms frame
	// wait + 10_000ms /quit exit wait (terminateAndRequireCleanExit). The
	// test timeout is their sum plus 50% headroom so a slow runner fails
	// the assertion, not the harness timeout.
	const FRAME_WAIT_DEADLINE_MS = 5_000;
	const QUIT_EXIT_DEADLINE_MS = 10_000;
	const IGNORE_QUIT_TEST_TIMEOUT_MS = Math.round(
		(FRAME_WAIT_DEADLINE_MS + QUIT_EXIT_DEADLINE_MS) * 1.5,
	);
	test("accepts an already-settled clean exit through the production helper", async () => {
		const sandbox = temporaryDirectory("perf-settled-clean-");
		const pty = spawnPty({
			argv: [bunExecutable, "-e", CLEAN_EXIT_CHILD],
			cwd: sandbox,
			size: { columns: 100, rows: 32 },
		});
		try {
			expect(await pty.waitForExit(5_000)).toBe(0);
			await terminateAndRequireCleanExit(pty, "first-frame:settled-clean");
		} finally {
			await pty.terminate();
		}
	}, 15_000);

	test("rejects a nonzero already-settled exit through the production helper", async () => {
		const sandbox = temporaryDirectory("perf-settled-failure-");
		const pty = spawnPty({
			argv: [bunExecutable, "-e", FAILURE_EXIT_CHILD],
			cwd: sandbox,
			size: { columns: 100, rows: 32 },
		});
		try {
			expect(await pty.waitForExit(5_000)).toBe(7);
			await expect(terminateAndRequireCleanExit(pty, "first-frame:settled-failure")).rejects.toBeInstanceOf(
				HarnessFailure,
			);
		} finally {
			await pty.terminate();
		}
	}, 15_000);

	test("rejects a child that emits a frame but ignores /quit", async () => {
		const sandbox = temporaryDirectory("perf-ignore-quit-");
		const pty = spawnPty({
			argv: [bunExecutable, "-e", IGNORE_QUIT_CHILD],
			cwd: sandbox,
			size: { columns: 100, rows: 32 },
		});
		try {
			await pty.waitFor((candidate) => frameObservation(candidate) !== undefined, {
				deadlineMs: FRAME_WAIT_DEADLINE_MS,
				source: "raw",
			});
			const frame = frameObservation(pty.snapshot());
			expect(frame).toBeDefined();
			expect(frame?.bytes).toBeGreaterThan(0);
			await expect(terminateAndRequireCleanExit(pty, "first-frame:ignore-quit")).rejects.toThrow(/did not exit through \/quit/);
		} finally {
			await pty.terminate();
		}
	}, IGNORE_QUIT_TEST_TIMEOUT_MS);

	test("passes a child that emits a frame and exits cleanly on /quit", async () => {
		const sandbox = temporaryDirectory("perf-clean-quit-");
		const pty = spawnPty({
			argv: [bunExecutable, "-e", CLEAN_QUIT_CHILD],
			cwd: sandbox,
			size: { columns: 100, rows: 32 },
		});
		try {
			const snapshot = await pty.waitFor((candidate) => frameObservation(candidate) !== undefined, {
				deadlineMs: 5_000,
				source: "raw",
			});
			const frame = frameObservation(snapshot);
			expect(frame).toBeDefined();
			expect(frame?.elapsedMs).toBeGreaterThanOrEqual(0);
			await terminateAndRequireCleanExit(pty, "first-frame:clean-quit");
			expect(pty.exited).toBe(true);
		} finally {
			await pty.terminate();
		}
	}, 15_000);
});
