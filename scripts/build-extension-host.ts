#!/usr/bin/env bun
/**
 * Standalone entrypoint for building the TypeScript extension host sidecar.
 *
 * Used by the CI matrix to build the host sidecar independently, and
 * internally by `package-release.ts`. Parses `--target`, compiles the host
 * and runtime-import fixture, verifies the hello handshake, and prints the
 * path to the resulting artifact (or fallback bundle).
 */

import { join, resolve } from "node:path";

import { parseReleaseArgs } from "./release/args.ts";
import { buildHost } from "./release/host.ts";
import { SpawnRunner } from "./release/runner.ts";

async function main(): Promise<void> {
	const args = parseReleaseArgs(process.argv.slice(2));
	const repoRoot = resolve(import.meta.dirname, "..");
	const stagingRoot = join(args.outDir, ".staging-host");

	process.stdout.write(
		`Building host for ${args.plan.bunTarget} (Rust target: ${args.plan.rustTarget})...\n`,
	);
	const host = await buildHost({
		repoRoot,
		stagingRoot,
		plan: args.plan,
		skipTests: args.skipHostTests,
		skipRuntimeImport: false,
		skipHandshake: !args.handshake,
		runner: new SpawnRunner(),
	});

	if (host.kind === "compiled") {
		process.stdout.write(`\nSuccess! Compiled host sidecar:\n`);
		process.stdout.write(`  ${host.binaryPath}\n`);
	} else {
		process.stdout.write(`\nSuccess! Runtime-bundle fallback:\n`);
		process.stdout.write(`  Script:  ${host.scriptPath}\n`);
		process.stdout.write(`  Runtime: ${host.runtimePath} (supplied by caller)\n`);
	}

	// Optional: we leave the staging directory intact so CI can upload it.
	// package-release.ts cleans it up during the atomic move.
}

main().catch((err: unknown) => {
	console.error(err);
	process.exit(1);
});
