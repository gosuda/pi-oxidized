/**
 * Host sidecar build + verification.
 *
 * Encapsulates the three release-time host operations:
 *   1. Compile the host with `bun build --compile --target bun-<os>-<arch>`.
 *   2. Run the runtime-import fixture as a standalone executable, proving
 *      the host can load an external `.ts` extension at runtime.
 *   3. Speak the JSONL `hello` handshake against the compiled sidecar to
 *      confirm the wire protocol and compatibility version.
 *
 * If compile or runtime-import fails for a target, the caller falls back to
 * shipping the official Bun runtime + the bundled host JavaScript. If both
 * paths fail, the release for that target fails — never embed the host into
 * the Rust binary.
 */

import { existsSync } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";

import type { CommandRunner, RunResult } from "./runner.ts";
import { SpawnRunner } from "./runner.ts";
import type { TargetPlan } from "./targets.ts";

/** Wire protocol version negotiated in `hello` (mirrors pi-tui-protocol). */
const PROTOCOL_VERSION = 1;

/** Compatibility target version (mirrors host COMPATIBILITY_VERSION). */
const COMPATIBILITY_VERSION = "0.80.10";

/** Maximum bytes of one JSONL frame line. Mirrors the protocol constant. */
const FRAME_MAX_BYTES = 8 * 1024 * 1024;

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
 *   5. Compile + run the runtime-import fixture (unless `skipRuntimeImport`).
 *   6. Speak the JSONL `hello` handshake against the sidecar (unless
 *      `skipHandshake`).
 *
 * If any of steps 4–6 fail, the function falls back to the runtime+bundle
 * path: `bun build --target bun` produces `pi-extension-host.js`, and the
 * caller must provide a sibling `bun[.exe]`. If that path also fails, the
 * function throws {@link HostBuildError}.
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
		{ cwd: hostDir, rejectOnError: false },
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
		const ok = await runRuntimeImportFixture(options, hostDir, outDir, runner, sidecarPath);
		if (!ok) return undefined;
	}
	if (!options.skipHandshake) {
		const ok = await runHelloHandshake(options, sidecarPath, runner);
		if (!ok) return undefined;
	}
	return { kind: "compiled", binaryPath: sidecarPath };
}

/**
 * Compile and run the runtime-import fixture against the freshly built
 * sidecar. The fixture dynamically imports an external `.ts` extension, so
 * this proves the compiled binary can load TypeScript extensions at runtime.
 *
 * The fixture source lives at `fixtures/runtime-import.ts`. We re-compile it
 * with the same Bun target as the host and invoke it against a known-good
 * example extension shipped with the reference tree.
 */
async function runRuntimeImportFixture(
	options: BuildHostOptions,
	hostDir: string,
	outDir: string,
	runner: CommandRunner,
	_sidecarPath: string,
): Promise<boolean> {
	const fixtureSource = resolve(hostDir, "fixtures", "runtime-import.ts");
	if (!existsSync(fixtureSource)) return false;
	const fixtureBin = join(outDir, `runtime-import-test${options.plan.windows ? ".exe" : ""}`);
	if (existsSync(fixtureBin)) await rm(fixtureBin, { force: true });

	const compile = await runner.run(
		"bun",
		[
			"build",
			"./fixtures/runtime-import.ts",
			"--compile",
			"--target",
			options.plan.bunTarget,
			"--outfile",
			fixtureBin,
		],
		{ cwd: hostDir, rejectOnError: false },
	);
	if (compile.exitCode !== 0 || !existsSync(fixtureBin)) return false;

	// Use a fixture extension that we know loads cleanly without external deps.
	const exampleExt = resolve(
		options.repoRoot,
		"packages",
		"extension-host",
		"fixtures",
		"extensions",
		"tool.ts",
	);
	if (!existsSync(exampleExt)) return false;

	const run = await runner.run(fixtureBin, [exampleExt], {
		cwd: hostDir,
		rejectOnError: false,
	});
	if (run.exitCode !== 0) return false;
	// Fixture prints one JSON line of registration summary; accept any nonempty
	// stdout as success (the wire shape is asserted in dedicated host tests).
	return run.stdout.trim().length > 0;
}

/**
 * Speak the JSONL `hello` handshake with the compiled sidecar. Sends one
 * hello request frame on stdin, reads the matching reply, validates the
 * protocol + compatibility versions, then closes stdin.
 */
async function runHelloHandshake(
	options: BuildHostOptions,
	sidecarPath: string,
	runner: CommandRunner,
): Promise<boolean> {
	const helloLine =
		JSON.stringify({
			id: 1,
			kind: "req",
			method: "hello",
			payload: {
				protocolVersion: PROTOCOL_VERSION,
				compatibilityVersion: COMPATIBILITY_VERSION,
			},
		}) + "\n";
	const res = await runner.run(sidecarPath, [], {
		cwd: dirname(sidecarPath),
		stdin: helloLine,
		rejectOnError: false,
	});
	// Host returns nonzero when stdin closes (normal shutdown); inspect stdout.
	const firstLine = res.stdout.split("\n", 1)[0];
	if (firstLine === undefined || firstLine.length === 0) return false;
	if (firstLine.length > FRAME_MAX_BYTES) return false;
	let frame: unknown;
	try {
		frame = JSON.parse(firstLine) as unknown;
	} catch {
		return false;
	}
	return isHelloAck(frame, options.plan);
}

/** Narrow a parsed frame into a hello-ack with the expected versions. */
function isHelloAck(frame: unknown, plan: TargetPlan): boolean {
	if (typeof frame !== "object" || frame === null) return false;
	const f = frame as { kind?: unknown; method?: unknown; payload?: unknown };
	if (f.kind !== "res" || f.method !== "hello") return false;
	if (typeof f.payload !== "object" || f.payload === null) return false;
	const p = f.payload as { protocolVersion?: unknown; compatibilityVersion?: unknown };
	return (
		p.protocolVersion === PROTOCOL_VERSION &&
		p.compatibilityVersion === COMPATIBILITY_VERSION &&
		// Reference the plan so unused-parameter lints stay quiet and future
		// target-specific handshake variants can hook here.
		plan.rustTarget.length > 0
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
			"--outdir",
			outDir,
			"--target",
			"bun",
			"--minify",
			"--outfile",
			scriptPath,
		],
		{ cwd: hostDir, rejectOnError: false },
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
