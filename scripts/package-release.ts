#!/usr/bin/env bun
/**
 * Master release script for pi-oxidized.
 *
 * Drives the cross-target release assembly defined in the master plan:
 *   1. `cargo build -p pi --release --locked --target <triple>`
 *   2. Host compile + runtime-import fixture + hello handshake (via host.ts).
 *   3. Artifact assembly (binary, host, docs, assets) into a staging tree.
 *   4. Verification (target agreement, executable bits, no host bytes in pi).
 *   5. Deterministic `.tar.gz` or `.zip` creation with `.sha256` sidecar.
 *   6. Copy artifacts into the final output directory and run unpack smoke.
 *
 * Usage:
 *   bun run scripts/package-release.ts --target <triple> [--out <dir>] [--dry-run]
 *
 *   --target <triple>        one of the five supported Rust triples
 *   --out / --out-dir <dir>  output directory (default: <cwd>/dist/release)
 *   --dry-run                skip cargo + host build; assemble from stub binaries
 *   --no-cargo               skip cargo build, but still compile host and archive
 *   --skip-host-tests        skip `bun test` inside the host package
 *   --no-handshake           skip the host `hello` handshake verification
 *   --source-date-epoch <s>  override SOURCE_DATE_EPOCH for archive mtimes
 *
 * Verification check 13 calls for, from the unpacked archive:
 *   - `pi --version` (binary runs and reports the workspace version)
 *   - host `hello` handshake (compiled sidecar responds with the expected version)
 *   - runtime-import fixture (external `.ts` extension loads through the sidecar)
 *
 * `smokeUnpacked` unpacks the finalized archive into a fresh directory before
 * running any check so archive corruption, missing files, or wrong target
 * fail the release.
 */

import { spawn } from "node:child_process";
import { mkdir, rm } from "node:fs/promises";
import { join, resolve } from "node:path";

import { checksumLine, sha256Bytes, writeTarGz, writeZip } from "./release/archive.ts";
import { parseReleaseArgs } from "./release/args.ts";
import { buildHost } from "./release/host.ts";
import { realFs, SpawnRunner, type CommandRunner, type Fs } from "./release/runner.ts";
import { pathExists } from "./release/runner.ts";
import { assembleRelease } from "./release/stage.ts";
import type { TargetPlan } from "./release/targets.ts";

/** Compatibility version stamped into the manifest and host hello payload. */
const COMPATIBILITY_VERSION = "0.80.10";

/** Protocol version stamped into the manifest and host hello payload. */
const PROTOCOL_VERSION = 1;

async function main(): Promise<void> {
	const args = parseReleaseArgs(process.argv.slice(2));
	const repoRoot = resolve(import.meta.dirname, "..");
	const runner = new SpawnRunner();
	const fs = realFs;

	process.stdout.write(`=== Building release for ${args.plan.rustTarget} ===\n`);
	process.stdout.write(`Bun host target: ${args.plan.bunTarget}\n`);
	process.stdout.write(`Archive format:  ${args.plan.archive}\n`);
	process.stdout.write(`Output dir:      ${args.outDir}\n`);
	process.stdout.write(
		`Mode:            ${args.dryRun ? "dry-run" : args.noCargo ? "no-cargo" : "full"}\n\n`,
	);

	const stagingRoot = join(args.outDir, `.staging-release-${args.plan.rustTarget}`);
	await fs.mkdir(stagingRoot, { recursive: true });

	const piBinaryPath = args.dryRun
		? join(stagingRoot, "pi-mock")
		: join(repoRoot, "target", args.plan.rustTarget, "release", args.plan.piBinaryName);

	try {
		if (args.dryRun) {
			await fs.writeFile(
				piBinaryPath,
				`mock-pi ${args.plan.rustTarget} source-date-epoch=${args.sourceDateEpoch}\n`,
			);
			process.stdout.write(`[1/6] Skipping cargo build (dry-run).\n`);
		} else if (args.noCargo) {
			if (!(await pathExists(fs, piBinaryPath))) {
				throw new Error(
					`--no-cargo requires a pre-built binary at ${piBinaryPath}. ` +
						`Run \`cargo build -p pi --release --locked --target ${args.plan.rustTarget}\` first.`,
				);
			}
			process.stdout.write(`[1/6] Using pre-built cargo artifact at ${piBinaryPath}.\n`);
		} else {
			process.stdout.write(`[1/6] Running cargo build --target ${args.plan.rustTarget}...\n`);
			const cargoRes = await runner.run(
				"cargo",
				["build", "-p", "pi", "--release", "--locked", "--target", args.plan.rustTarget],
				{ cwd: repoRoot },
			);
			if (cargoRes.exitCode !== 0) {
				throw new Error(`Cargo build failed: ${cargoRes.stderr.slice(0, 2000)}`);
			}
		}

		// 2. Host sidecar.
		process.stdout.write(`[2/6] Building extension host sidecar...\n`);
		const host = args.dryRun
			? {
					kind: "compiled" as const,
					binaryPath: join(stagingRoot, args.plan.hostBinaryName),
				}
			: await buildHost({
					repoRoot,
					stagingRoot,
					plan: args.plan,
					skipTests: args.skipHostTests,
					skipRuntimeImport: false,
					skipHandshake: !args.handshake,
					runner,
				});

		if (args.dryRun && host.kind === "compiled") {
			await fs.writeFile(
				host.binaryPath,
				`mock-host ${args.plan.bunTarget} source-date-epoch=${args.sourceDateEpoch}\n`,
			);
		}

		// Optional runtime for the runtime-bundle fallback path. In dry-run we
		// write a mock so assembly completes; otherwise the caller is
		// responsible for placing the official Bun runtime alongside the host.
		const bunRuntimePath = host.kind === "runtime-bundle"
			? join(stagingRoot, args.plan.bunRuntimeName)
			: undefined;
		if (args.dryRun && bunRuntimePath !== undefined) {
			await fs.writeFile(
				bunRuntimePath,
				`mock-bun-runtime ${args.plan.bunTarget} source-date-epoch=${args.sourceDateEpoch}\n`,
			);
		}

		// 3. Assembly & verification.
		process.stdout.write(`[3/6] Assembling artifacts and verifying invariants...\n`);
		const pkgJson = JSON.parse(
			new TextDecoder().decode(await fs.readFile(join(repoRoot, "package.json"))),
		) as { version: string };

		const assembly = await assembleRelease(stagingRoot, {
			plan: args.plan,
			version: pkgJson.version,
			piBinaryPath,
			repoRoot,
			host,
			bunRuntimePath,
			fs,
			sourceDateEpoch: parseInt(args.sourceDateEpoch, 10),
			compatibilityVersion: COMPATIBILITY_VERSION,
			protocolVersion: PROTOCOL_VERSION,
			createdAt: new Date(parseInt(args.sourceDateEpoch, 10) * 1000).toISOString(),
			docsSource: join(repoRoot, "crates", "pi", "docs"),
			examplesSource: join(
				repoRoot,
				".references",
				"pi",
				"packages",
				"coding-agent",
				"examples",
			),
			assetsSource: join(repoRoot, "crates", "pi", "assets"),
		});

		// 4. Deterministic archive.
		process.stdout.write(`[4/6] Creating deterministic archive...\n`);
		const archiveBase = `pi-${pkgJson.version}-${args.plan.archiveDir}.${args.plan.archive}`;
		const archivePath = join(stagingRoot, archiveBase);
		const entries: { path: string; data: Uint8Array; mode: number }[] = [];
		for (const file of assembly.manifest.files) {
			const data = await fs.readFile(join(assembly.stagingDir, file.path));
			entries.push({
				path: `${args.plan.archiveDir}/${file.path}`,
				data,
				mode: file.executable ? 0o755 : 0o644,
			});
		}
		const archiveOpts = { sourceDateEpoch: parseInt(args.sourceDateEpoch, 10) };
		if (args.plan.archive === "zip") {
			await writeZip(entries, archivePath, archiveOpts);
		} else {
			await writeTarGz(entries, archivePath, archiveOpts);
		}

		// 5. Checksum sidecar.
		const archiveBytes = await fs.readFile(archivePath);
		const digest = sha256Bytes(archiveBytes);
		const checksumPath = join(stagingRoot, `${archiveBase}.sha256`);
		await fs.writeFile(checksumPath, checksumLine(digest, archiveBase));

		// 6. Finalization.
		process.stdout.write(`[5/6] Finalizing release in ${args.outDir}...\n`);
		await fs.mkdir(args.outDir, { recursive: true });
		const finalArchivePath = join(args.outDir, archiveBase);
		const finalChecksumPath = join(args.outDir, `${archiveBase}.sha256`);
		await fs.copyFile(archivePath, finalArchivePath);
		await fs.copyFile(checksumPath, finalChecksumPath);

		// 7. Unpack smoke (verification check 13). We always extract the
		// finalized archive so a wrong target, missing host, or empty pi
		// binary fails the release.
		process.stdout.write(`[6/6] Running unpack smoke checks...\n`);
		const smokeRoot = join(args.outDir, `.smoke-${args.plan.rustTarget}`);
		await rm(smokeRoot, { recursive: true, force: true });
		await mkdir(smokeRoot, { recursive: true });
		try {
			await unpackArchive(archivePath, args.plan, smokeRoot);
			await smokeUnpacked({
				fs,
				runner,
				archiveDir: join(smokeRoot, args.plan.archiveDir),
				plan: args.plan,
				repoRoot,
				dryRun: args.dryRun,
			});
		} finally {
			await rm(smokeRoot, { recursive: true, force: true });
		}

		process.stdout.write(`\n=== Release Complete ===\n`);
		process.stdout.write(`Archive:  ${finalArchivePath}\n`);
		process.stdout.write(`Checksum: ${finalChecksumPath}\n`);
	} finally {
		await fs.rm(stagingRoot, { recursive: true, force: true });
	}
}

/** Inputs to {@link smokeUnpacked}. */
interface SmokeOptions {
	readonly fs: Fs;
	readonly runner: CommandRunner;
	/** Absolute path to the unpacked archive root (containing `pi`, etc.). */
	readonly archiveDir: string;
	readonly plan: TargetPlan;
	readonly repoRoot: string;
	readonly dryRun: boolean;
}

/**
 * Extract the finalized archive using the host's `tar` / `unzip`. The
 * archive layout is always `<archiveDir>/<file>`; the smoke step expects
 * `archiveDir` to point at that inner directory after extraction.
 */
async function unpackArchive(
	archivePath: string,
	plan: TargetPlan,
	smokeRoot: string,
): Promise<void> {
	if (plan.archive === "zip") {
		await runHostTool(["unzip", "-q", archivePath, "-d", smokeRoot]);
	} else {
		await runHostTool(["tar", "-xzf", archivePath, "-C", smokeRoot]);
	}
}

/**
 * Invoke a host shell tool (`tar`, `unzip`) via `node:child_process`. These
 * are POSIX utilities available on the dev host and CI runners, so we don't
 * pull in another zip/tar implementation just for the smoke step.
 */
function runHostTool(args: readonly string[]): Promise<void> {
	const { promise, resolve, reject } = Promise.withResolvers<void>();
	const child = spawn(args[0] ?? "", args.slice(1), { stdio: ["ignore", "pipe", "pipe"] });
	let stderr = "";
	child.stderr?.on("data", (chunk: Buffer) => {
		stderr += chunk.toString("utf8");
	});
	child.on("error", (err: Error) => reject(err));
	child.on("close", (code: number | null) => {
		if (code === 0) {
			resolve();
		} else {
			reject(new Error(`${args[0]} exited ${code}: ${stderr.slice(0, 500)}`));
		}
	});
	return promise;
}

/**
 * Run the three verification check 13 smoke checks against the unpacked
 * archive. Real failures throw; architectural mismatches against the dev
 * host (e.g. extracting a darwin archive on linux) skip with a clear note.
 */
async function smokeUnpacked(opts: SmokeOptions): Promise<void> {
	const { fs, runner, archiveDir, plan, repoRoot, dryRun } = opts;
	const piInArchive = join(archiveDir, plan.piBinaryName);
	if (!(await pathExists(fs, piInArchive))) {
		throw new Error(`unpack smoke: missing ${plan.piBinaryName} inside ${archiveDir}`);
	}

	// In dry-run the staged `pi` is a stub text file, not a real executable,
	// so we cannot invoke it. Verify the archive structure and skip the
	// process-level checks.
	if (dryRun) {
		const stat = await fs.stat(piInArchive);
		process.stdout.write(
			`  [dry-run] ${plan.piBinaryName}: present (${stat.size} bytes) — subprocess skipped\n`,
		);
		const hostBin = join(archiveDir, plan.hostBinaryName);
		if (await pathExists(fs, hostBin)) {
			const hstat = await fs.stat(hostBin);
			process.stdout.write(
				`  [dry-run] ${plan.hostBinaryName}: present (${hstat.size} bytes) — handshake skipped\n`,
			);
		} else {
			process.stdout.write(
				`  [dry-run] ${plan.hostBinaryName}: absent (runtime-bundle fallback)\n`,
			);
		}
		return;
	}

	const versionRes = await runner.run(piInArchive, ["--version"], {
		rejectOnError: false,
		cwd: archiveDir,
	});
	if (versionRes.exitCode !== 0) {
		throw new Error(
			`unpack smoke: pi --version failed (exit ${versionRes.exitCode}): ${versionRes.stderr.slice(0, 500)}`,
		);
	}
	process.stdout.write(`  pi --version: ${versionRes.stdout.trim() || "(no output)"}\n`);
	// Handshake against the compiled sidecar only (runtime-bundle has no
	// standalone binary). The handshake is the strongest proof the host is
	// both binary-buildable and protocol-compatible.
	const hostBin = join(archiveDir, plan.hostBinaryName);
	if (!(await pathExists(fs, hostBin))) {
		process.stdout.write(
			`  [skip] host hello handshake: ${plan.hostBinaryName} not in archive (runtime-bundle target)\n`,
		);
		return;
	}
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
	const hostRes = await runner.run(hostBin, [], {
		stdin: helloLine,
		rejectOnError: false,
		cwd: archiveDir,
	});
	const firstLine = hostRes.stdout.split("\n", 1)[0] ?? "";
	const ok =
		firstLine.includes('"method":"hello"') &&
		firstLine.includes('"kind":"res"') &&
		firstLine.includes(`"protocolVersion":${PROTOCOL_VERSION}`);
	if (!ok) {
		throw new Error(
			`unpack smoke: host hello handshake failed (exit ${hostRes.exitCode}): ${firstLine.slice(0, 500)}`,
		);
	}
	process.stdout.write(`  host hello handshake: OK\n`);

	// Runtime-import fixture: only meaningful for native architectures
	// (linux/macos/windows) and only when a `.ts` extension fixture is
	// present in the workspace. When the dev host cannot execute the
	// target binary we skip rather than fail.
	const targetMatchesHost = targetCompatibleWithDevHost(plan);
	if (!targetMatchesHost) {
		process.stdout.write(
			`  [skip] runtime-import fixture: dev host cannot execute ${plan.rustTarget}\n`,
		);
		return;
	}
	const exampleExt = join(
		repoRoot,
		"packages",
		"extension-host",
		"fixtures",
		"extensions",
		"tool.ts",
	);
	if (!(await pathExists(fs, exampleExt))) {
		process.stdout.write(`  [skip] runtime-import fixture: missing ${exampleExt}\n`);
		return;
	}
	const fixtureBin = join(archiveDir, `runtime-import-test${plan.windows ? ".exe" : ""}`);
	const fixtureRes = await runner.run(fixtureBin, [exampleExt], {
		rejectOnError: false,
		cwd: repoRoot,
	});
	if (fixtureRes.exitCode !== 0) {
		throw new Error(
			`unpack smoke: runtime-import fixture failed (exit ${fixtureRes.exitCode}): ${fixtureRes.stderr.slice(0, 500)}`,
		);
	}
	process.stdout.write(`  runtime-import fixture: OK\n`);
}

/** True when the host running this script can execute the target's binaries. */
function targetCompatibleWithDevHost(plan: TargetPlan): boolean {
	const dev = process.platform;
	if (plan.os === "windows" && dev !== "win32") return false;
	if (plan.os === "darwin" && dev !== "darwin") return false;
	if (plan.os === "linux" && dev !== "linux") return false;
	// We do not pin the dev CPU architecture; assume linux-x64 CI runs the
	// x86_64 and aarch64 (via qemu) targets if needed.
	return true;
}

main().catch((err: unknown) => {
	console.error(err);
	process.exit(1);
});
