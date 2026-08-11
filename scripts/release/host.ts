/**
 * Host sidecar build + verification.
 *
 * Encapsulates the three release-time host operations:
 *   1. Compile the host with `bun build --compile --target bun-<os>-<arch>`.
 *   2. Drive the compiled sidecar through `hello` and `extensions.load`,
 *      proving that the released artifact loads an external TypeScript extension.
 *   3. Speak an independent JSONL `hello` handshake against the sidecar.
 *
 * A target that cannot produce a compiled sidecar falls back to the official
 * Bun runtime plus the bundled host JavaScript. Once compilation succeeds,
 * probe or handshake failures are fatal: substituting a different runtime
 * graph would make release verification a false green.
 */

import { existsSync } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";

import type { CommandRunner, RunResult } from "./runner.ts";
import { SpawnRunner } from "./runner.ts";
import type { TargetPlan } from "./targets.ts";

/** Wire protocol version negotiated in `hello` (mirrors pi-tui-protocol). */
export const HOST_PROTOCOL_VERSION = 1;

/** Compatibility target version (mirrors host COMPATIBILITY_VERSION). */
export const HOST_COMPATIBILITY_VERSION = "0.80.10";

/** Maximum bytes of one JSONL frame line. Mirrors the protocol constant. */
const FRAME_MAX_BYTES = 8 * 1024 * 1024;

const HOST_SETUP_TIMEOUT_MS = 5 * 60_000;
const HOST_BUILD_TIMEOUT_MS = 10 * 60_000;
const HOST_PROBE_TIMEOUT_MS = 30_000;

/**
 * The artifact set the release script must place beside `pi`. The Rust
 * binary resolves the host by trying, in order: `PI_EXTENSION_HOST` env,
 * then the sibling `pi-extension-host[.exe]`, then (fallback) sibling
 * `bun[.exe] pi-extension-host.js`.
 */
export type HostArtifact =
	| { readonly kind: "compiled"; readonly binaryPath: string }
	| {
			readonly kind: "runtime-bundle";
			readonly runtimePath: string;
			readonly scriptPath: string;
	  };

/** Inputs to {@link buildHost}. */
export interface BuildHostOptions {
	/** Absolute path to the workspace root (contains `packages/extension-host`). */
	readonly repoRoot: string;
	/** Resolved release target. */
	readonly plan: TargetPlan;
	/** Working directory under which artifacts are emitted (a `host/` subdir is created). */
	readonly stagingRoot: string;
	/** Skip the host unit tests (`bun test`). */
	readonly skipTests: boolean;
	/** Skip the runtime-import fixture run. */
	readonly skipRuntimeImport: boolean;
	/** Skip the `hello` handshake against the compiled sidecar. */
	readonly skipHandshake: boolean;
	/** Command runner. Defaults to {@link SpawnRunner}. */
	readonly runner?: CommandRunner;
}

/** Error raised when host build/verify fails on every available path. */
export class HostBuildError extends Error {
	readonly target: string;
	constructor(target: string, message: string) {
		super(`Host build failed for ${target}: ${message}`);
		this.name = "HostBuildError";
		this.target = target;
	}
}

/** Absolute path to the host package directory inside the workspace. */
function hostPackageDir(repoRoot: string): string {
	return resolve(repoRoot, "packages/extension-host");
}

/** Absolute path to a per-target staging subdirectory. */
function targetStagingDir(stagingRoot: string, plan: TargetPlan): string {
	return resolve(stagingRoot, "host", plan.rustTarget);
}

/**
 * Build (and verify) the host sidecar for `plan`.
 *
 * Algorithm:
 *   1. `bun install --frozen-lockfile` (host deps are source-pinned).
 *   2. `tsc --noEmit` (host typecheck).
 *   3. `bun test` (unless `skipTests`).
 *   4. `bun build --compile --target <plan.bunTarget>` → sidecar binary.
 *   5. Run the source probe against that binary: `hello`, then
 *      `extensions.load` of the Type-based tool fixture.
 *   6. Speak an independent JSONL `hello` handshake (unless
 *      `skipHandshake`).
 *
 * If step 4 cannot create a compiled artifact, the function falls back to the
 * runtime+bundle path. A compiled artifact that fails either behavioral probe
 * fails the release instead of substituting a different runtime graph.
 */
export async function buildHost(options: BuildHostOptions): Promise<HostArtifact> {
	const runner = options.runner ?? new SpawnRunner();
	const hostDir = hostPackageDir(options.repoRoot);
	const outDir = targetStagingDir(options.stagingRoot, options.plan);
	await mkdir(outDir, { recursive: true });

	await installHostDeps(hostDir, runner);
	await typecheckHost(hostDir, runner);
	if (!options.skipTests) await testHost(hostDir, runner);

	const compiled = await tryCompiledPath(options, hostDir, outDir, runner);
	if (compiled !== undefined) return compiled;

	const bundled = await tryRuntimeBundlePath(options, hostDir, outDir, runner);
	if (bundled !== undefined) return bundled;

	throw new HostBuildError(
		options.plan.rustTarget,
		"compiled sidecar and runtime-bundle fallback both failed",
	);
}

/** Install host dependencies with the locked file. */
async function installHostDeps(hostDir: string, runner: CommandRunner): Promise<void> {
	const res = await runner.run("bun", ["install", "--frozen-lockfile"], {
		cwd: hostDir,
		rejectOnError: false,
		timeoutMs: HOST_SETUP_TIMEOUT_MS,
	});
	if (res.exitCode !== 0) {
		throw new HostBuildError(
			hostDir,
			`bun install --frozen-lockfile failed (exit ${res.exitCode}); run 'bun install' in packages/extension-host and commit the refreshed lockfile. stderr=${res.stderr.slice(0, 500)}`,
		);
	}
}

/** Run `tsc --noEmit` against the host's `tsconfig.check.json`. */
async function typecheckHost(hostDir: string, runner: CommandRunner): Promise<void> {
	const res = await runner.run("bun", ["run", "check"], {
		cwd: hostDir,
		rejectOnError: false,
		timeoutMs: HOST_SETUP_TIMEOUT_MS,
	});
	if (res.exitCode !== 0) {
		throw new HostBuildError(
			hostDir,
			`host typecheck failed (exit ${res.exitCode}). stderr=${res.stderr.slice(0, 1000)}`,
		);
	}
}

/** Run `bun test` in the host package. */
async function testHost(hostDir: string, runner: CommandRunner): Promise<void> {
	const res = await runner.run("bun", ["run", "test"], {
		cwd: hostDir,
		rejectOnError: false,
		timeoutMs: HOST_BUILD_TIMEOUT_MS,
	});
	if (res.exitCode !== 0) {
		throw new HostBuildError(
			hostDir,
			`host tests failed (exit ${res.exitCode}). stderr=${res.stderr.slice(0, 1000)}`,
		);
	}
}

/** Build the compiled sidecar with `--target bun-<os>-<arch>[-baseline]`. */
async function compileSidecar(
	hostDir: string,
	outPath: string,
	plan: TargetPlan,
	runner: CommandRunner,
): Promise<RunResult> {
	return runner.run(
		"bun",
		[
			"build",
			"./src/main.ts",
			"--compile",
			"--minify",
			"--compile-autoload-tsconfig",
			"--compile-autoload-package-json",
			"--target",
			plan.bunTarget,
			"--outfile",
			outPath,
		],
		{ cwd: hostDir, rejectOnError: false, timeoutMs: HOST_BUILD_TIMEOUT_MS },
	);
}

/**
 * Try the compiled-sidecar path. Returns the artifact on success, or
 * `undefined` if any step fails (caller falls back).
 */
async function tryCompiledPath(
	options: BuildHostOptions,
	hostDir: string,
	outDir: string,
	runner: CommandRunner,
): Promise<HostArtifact | undefined> {
	const sidecarPath = join(outDir, options.plan.hostBinaryName);
	if (existsSync(sidecarPath)) await rm(sidecarPath, { force: true });

	const compileRes = await compileSidecar(hostDir, sidecarPath, options.plan, runner);
	if (compileRes.exitCode !== 0 || !existsSync(sidecarPath)) return undefined;

	if (!options.skipRuntimeImport) {
		const error = await probeRuntimeImport(hostDir, sidecarPath, runner);
		if (error !== undefined) {
			throw new HostBuildError(options.plan.rustTarget, error);
		}
	}
	if (!options.skipHandshake) {
		const ok = await runHelloHandshake(sidecarPath, runner);
		if (!ok) {
			throw new HostBuildError(
				options.plan.rustTarget,
				"compiled sidecar hello handshake failed",
			);
		}
	}
	return { kind: "compiled", binaryPath: sidecarPath };
}

/**
 * Drive the actual compiled sidecar through a correlated `hello` and
 * `extensions.load` exchange. The source probe stays outside the release
 * artifact, so this verifies the binary that will be packaged instead of a
 * separately compiled copy of the extension runtime.
 */
async function probeRuntimeImport(
	hostDir: string,
	sidecarPath: string,
	runner: CommandRunner,
): Promise<string | undefined> {
	const fixtureSource = resolve(hostDir, "fixtures", "runtime-import.ts");
	const exampleExt = resolve(hostDir, "fixtures", "extensions", "tool.ts");
	if (!existsSync(fixtureSource)) {
		return `runtime-import probe missing at ${fixtureSource}`;
	}
	if (!existsSync(exampleExt)) {
		return `runtime-import extension missing at ${exampleExt}`;
	}
	const run = await runner.run(
		"bun",
		[fixtureSource, sidecarPath, exampleExt],
		{
			cwd: hostDir,
			rejectOnError: false,
			timeoutMs: HOST_PROBE_TIMEOUT_MS,
		},
	);
	if (run.exitCode !== 0) {
		return `runtime-import probe failed (exit ${run.exitCode}): ${run.stderr.slice(-1000)}`;
	}
	const lines = run.stdout.split("\n").filter((line) => line.length > 0);
	if (lines.length !== 1) {
		return `runtime-import probe emitted ${lines.length} stdout lines`;
	}
	let parsed: unknown;
	try {
		parsed = JSON.parse(lines[0] ?? "");
	} catch (error) {
		return `runtime-import probe returned invalid JSON: ${String(error)}`;
	}
	if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
		return "runtime-import probe returned a non-object summary";
	}
	const summary = parsed as { path?: unknown; tools?: unknown };
	if (
		summary.path !== exampleExt ||
		!Array.isArray(summary.tools) ||
		!summary.tools.includes("echo")
	) {
		return "runtime-import probe returned the wrong extension summary";
	}
	return undefined;
}

/**
 * Speak the JSONL `hello` handshake with the compiled sidecar. Sends one
 * hello request frame on stdin, reads the matching reply, validates the
 * protocol + compatibility versions, then closes stdin.
 */
async function runHelloHandshake(
	sidecarPath: string,
	runner: CommandRunner,
): Promise<boolean> {
	const res = await runner.run(sidecarPath, [], {
		cwd: dirname(sidecarPath),
		stdin: helloRequestLine(),
		rejectOnError: false,
		timeoutMs: HOST_PROBE_TIMEOUT_MS,
	});
	const firstLine = res.stdout.split("\n", 1)[0] ?? "";
	return res.exitCode === 0 && isHelloAckLine(firstLine);
}

/** Canonical JSONL request used by build-time and unpacked-archive probes. */
export function helloRequestLine(): string {
	return `${JSON.stringify({
		id: 1,
		kind: "req",
		method: "hello",
		payload: {
			protocolVersion: HOST_PROTOCOL_VERSION,
			compatibilityVersion: HOST_COMPATIBILITY_VERSION,
		},
	})}\n`;
}

/** Parse one line and validate the complete hello acknowledgement contract. */
export function isHelloAckLine(line: string): boolean {
	if (line.length === 0 || line.length > FRAME_MAX_BYTES) return false;
	let frame: unknown;
	try {
		frame = JSON.parse(line) as unknown;
	} catch {
		return false;
	}
	return isHelloAck(frame);
}

/** Narrow a parsed frame into a hello-ack with the expected versions. */
export function isHelloAck(frame: unknown): boolean {
	if (typeof frame !== "object" || frame === null) return false;
	const f = frame as { kind?: unknown; method?: unknown; payload?: unknown };
	if (f.kind !== "res" || f.method !== "hello") return false;
	if (typeof f.payload !== "object" || f.payload === null) return false;
	const p = f.payload as { protocolVersion?: unknown; compatibilityVersion?: unknown };
	return (
		p.protocolVersion === HOST_PROTOCOL_VERSION &&
		p.compatibilityVersion === HOST_COMPATIBILITY_VERSION
	);
}

/**
 * Fallback path: bundle the host as plain JS for the platform's `bun`
 * runtime. Returns the artifact pair, or `undefined` if bundling fails.
 *
 * The caller (release script) is responsible for placing a `bun[.exe]`
 * binary next to `pi-extension-host.js` in the archive.
 */
async function tryRuntimeBundlePath(
	options: BuildHostOptions,
	hostDir: string,
	outDir: string,
	runner: CommandRunner,
): Promise<HostArtifact | undefined> {
	const scriptPath = join(outDir, options.plan.hostBundleName);
	if (existsSync(scriptPath)) await rm(scriptPath, { force: true });
	const res = await runner.run(
		"bun",
		[
			"build",
			"./src/main.ts",
			"--target",
			"bun",
			"--minify",
			"--outfile",
			scriptPath,
		],
		{ cwd: hostDir, rejectOnError: false, timeoutMs: HOST_BUILD_TIMEOUT_MS },
	);
	if (res.exitCode !== 0 || !existsSync(scriptPath)) return undefined;
	const runtimePath = join(outDir, options.plan.bunRuntimeName);
	// Caller is responsible for populating `runtimePath` (from the official
	// Bun release for this target) before archiving. We just record the path.
	await writeFile(
		join(outDir, "runtime-bundle.txt"),
		`fallback\nscript=${scriptPath}\nruntime=${runtimePath}\ntarget=${options.plan.bunTarget}\n`,
		"utf8",
	);
	return { kind: "runtime-bundle", runtimePath, scriptPath };
}

/** Drop the per-target staging directory created by {@link buildHost}. */
export async function cleanupHostStaging(stagingRoot: string, plan: TargetPlan): Promise<void> {
	await rm(targetStagingDir(stagingRoot, plan), { recursive: true, force: true });
}
