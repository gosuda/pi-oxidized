import { afterAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
	assertPinnedBuildScriptContracts,
	buildProducts,
	HarnessFailure,
	loadPinnedBuildScriptManifests,
	referenceBuildCommands,
	requireCleanExitIfSettled,
	terminateAndRequireCleanExit,
} from "./performance.ts";
import { spawnPty } from "./pty.ts";
import type { PtySnapshot } from "./pty.ts";

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


describe("pinned reference build contract", () => {
	test("accepts the current pinned manifests", () => {
		expect(() => assertPinnedBuildScriptContracts(loadPinnedBuildScriptManifests())).not.toThrow();
	});

	test("keeps generate-models out of the hand-expanded command plan", () => {
		const commands = referenceBuildCommands("npm", "bun");
		expect(commands.flatMap((command) => command.argv)).not.toContain("generate-models");
		expect(commands.map((command) => command.label)).toContain("TypeScript pi ai build (generate-models skipped)");
	});

	for (const [packageName, script] of [
		["tui", "build"],
		["ai", "build"],
		["agent", "build"],
		["coding-agent", "build"],
		["coding-agent", "build:binary"],
		["coding-agent", "copy-binary-assets"],
	] as const) {
		test(`rejects drift in ${packageName} scripts.${script}`, () => {
			const manifests = loadPinnedBuildScriptManifests();
			const manifest = manifests[packageName] as { scripts: Record<string, string> };
			manifest.scripts[script] = `${manifest.scripts[script]} && echo drift`;
			expect(() => assertPinnedBuildScriptContracts(manifests)).toThrow(/reference build contract/);
		});
	}

	test("rejects manifest drift before executing a build command", async () => {
		const manifests = loadPinnedBuildScriptManifests();
		const manifest = manifests.ai as { scripts: Record<string, string> };
		manifest.scripts.build = `${manifest.scripts.build} && echo drift`;
		let commandCount = 0;
		await expect(
			buildProducts({
				manifests,
				runCommand: async () => {
					commandCount += 1;
				},
			}),
		).rejects.toThrow(/reference build contract/);
		expect(commandCount).toBe(0);
	});
});

test("does not run the benchmark when performance verification is imported", () => {
	const sandbox = temporaryDirectory("perf-import-");
	const artifactBefore = existsSync(PERFORMANCE_ARTIFACT)
		? readFileSync(PERFORMANCE_ARTIFACT)
		: undefined;
	try {
		// A separate process is required to observe module-entry side effects.
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

function frameObservation(snapshot: PtySnapshot): { elapsedMs: number; bytes: number } | undefined {
	let raw = "";
	let bytes = 0;
	for (const chunk of snapshot.chunks) {
		if (chunk.stream !== "pty") continue;
		raw += chunk.text;
		bytes += chunk.bytes.byteLength;
		const begin = raw.indexOf(SYNC_BEGIN);
		if (begin >= 0) {
			if (raw.indexOf(SYNC_END, begin + SYNC_BEGIN.length) >= 0) {
				return { elapsedMs: chunk.elapsedMs, bytes };
			}
			continue;
		}
	}
	return undefined;
}


// A child that emits a synchronized-output frame and then exits with code 0
// upon receiving any input (honoring /quit). This is the passing case.
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

// A child that emits a synchronized-output frame and then ignores /quit,
// keeping an interval alive so only a force-kill can remove it. This is the
// failing case: the verifier must reject it because /quit did not produce a
// clean exit, and the finally kill is cleanup-only, never success.
const IGNORE_QUIT_CHILD = `
process.stdout.write(${JSON.stringify(SYNC_BEGIN)} + "frame" + ${JSON.stringify(SYNC_END)} + "\\n");
process.stdin.setEncoding("utf8");
process.stdin.resume();
setInterval(() => {}, 1_000);
`;

const CLEAN_EXIT_CHILD = "process.exit(0);";
const FAILURE_EXIT_CHILD = "process.exit(7);";

describe.skipIf(isWindows)("performance first-frame lifecycle", () => {
	test("accepts an already-settled clean exit through the production helper", async () => {
		const sandbox = temporaryDirectory("perf-settled-clean-");
		const pty = spawnPty({
			argv: [bunExecutable, "-e", CLEAN_EXIT_CHILD],
			cwd: sandbox,
			size: { columns: 100, rows: 32 },
		});
		try {
			expect(await pty.waitForExit(5_000)).toBe(0);
			expect(await requireCleanExitIfSettled(pty, "first-frame:settled-clean")).toBe(true);
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
			await expect(requireCleanExitIfSettled(pty, "first-frame:settled-failure")).rejects.toBeInstanceOf(
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
				deadlineMs: 5_000,
				source: "raw",
			});
			const frame = frameObservation(pty.snapshot());
			expect(frame).toBeDefined();
			expect(frame?.bytes).toBeGreaterThan(0);
			await expect(terminateAndRequireCleanExit(pty, "first-frame:ignore-quit")).rejects.toThrow(/did not exit through \/quit/);
		} finally {
			await pty.terminate();
		}
	}, 15_000);

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
			// Must not throw: /quit drives a clean exit.
			await terminateAndRequireCleanExit(pty, "first-frame:clean-quit");
			expect(pty.exited).toBe(true);
		} finally {
			await pty.terminate();
		}
	}, 15_000);
});
