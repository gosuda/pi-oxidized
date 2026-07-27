#!/usr/bin/env bun
/**
 * Offline cross-version session interoperability verifier.
 *
 * Generated source-pinned fixtures are opened and evolved by Rust's
 * SessionManager, then every resulting JSONL is reopened by the pinned
 * TypeScript SessionManager without rewriting its historical prefix.
 */

import { readdir, readFile, rm } from "node:fs/promises";
import { join, relative, resolve } from "node:path";

const REPO_ROOT = resolve(import.meta.dirname, "../..");
const OUTPUT = join(REPO_ROOT, "target/verification/session-interop");
const REFERENCE_ROOT = join(REPO_ROOT, ".references/pi");
const REFERENCE_UUID = join(REFERENCE_ROOT, "packages/agent/src/harness/session/uuid.ts");
const REFERENCE_AI_TYPES = join(REFERENCE_ROOT, "packages/ai/src/types.ts");

export const SESSION_INTEROP_TIMEOUT_MS = 900_000;

interface SessionManagerLike {
	getHeader(): Record<string, unknown> | null;
	getEntries(): readonly Record<string, unknown>[];
	getTree(): readonly unknown[];
	buildSessionContext(): { messages: readonly unknown[] };
	getLeafId(): string | null;
}

interface SessionManagerStatic {
	open(path: string): SessionManagerLike;
}

function assert(condition: unknown, message: string): asserts condition {
	if (!condition) throw new Error(message);
}

function installReferenceResolver(): void {
	Bun.plugin({
		name: "source-pinned-pi-reference",
		setup(build) {
			build.onResolve({ filter: /^@earendil-works\/pi-agent-core$/ }, () => ({
				path: REFERENCE_UUID,
			}));
			build.onResolve({ filter: /^@earendil-works\/pi-ai$/ }, () => ({
				path: REFERENCE_AI_TYPES,
			}));
			build.onResolve({ filter: /^cross-spawn$/ }, () => ({
				path: "cross-spawn",
				namespace: "pi-shim",
			}));
			build.onLoad({ filter: /.*/, namespace: "pi-shim" }, () => ({
				contents:
					'import { spawn, spawnSync } from "node:child_process";\nexport default Object.assign(spawn, { sync: spawnSync });\n',
				loader: "js",
			}));
		},
	});
}

async function referenceSessionManager(): Promise<SessionManagerStatic> {
	installReferenceResolver();
	// Bun plugins register at runtime, so this source-pinned import cannot be static.
	const module = (await import(
		join(REFERENCE_ROOT, "packages/coding-agent/src/core/session-manager.ts")
	)) as { SessionManager?: SessionManagerStatic };
	assert(module.SessionManager !== undefined, "source-pinned TypeScript SessionManager is unavailable");
	return module.SessionManager;
}

async function jsonlFiles(root: string): Promise<string[]> {
	const files: string[] = [];
	for (const entry of await readdir(root, { withFileTypes: true })) {
		const path = join(root, entry.name);
		if (entry.isDirectory()) files.push(...(await jsonlFiles(path)));
		else if (entry.isFile() && entry.name.endsWith(".jsonl")) files.push(path);
	}
	return files.sort();
}

async function run(argv: string[], env?: Record<string, string>): Promise<void> {
	const process = Bun.spawn(argv, {
		cwd: REPO_ROOT,
		env: { ...Bun.env, ...env },
		stdout: "inherit",
		stderr: "inherit",
	});
	const exitCode = await process.exited;
	assert(exitCode === 0, `${argv.join(" ")} exited ${exitCode}`);
}

export async function reopenWithSourcePinnedTypescript(
	root: string,
	options: { preserveHistoricalPrefix?: boolean } = {},
): Promise<number> {
	const preserveHistoricalPrefix = options.preserveHistoricalPrefix ?? true;
	const SessionManager = await referenceSessionManager();
	const files = await jsonlFiles(root);
	assert(files.length > 0, `Rust proof did not produce session files under ${root}`);
	let fixtureContainsOpaqueFutureEntry = false;
	let sourcePinnedManagerRetainsOpaqueFutureEntry = false;

	for (const file of files) {
		const before = await readFile(file);
		const manager = SessionManager.open(file);
		const after = await readFile(file);
		const afterLines = after.toString().trim().split("\n");
		const finalEntry = JSON.parse(afterLines.at(-1)!) as { id?: unknown };
		assert(typeof finalEntry.id === "string", `TypeScript output has no final entry id: ${file}`);
		fixtureContainsOpaqueFutureEntry ||= before.includes(Buffer.from('"type":"future_thing"'));
		if (preserveHistoricalPrefix) {
			assert(
				Buffer.compare(before, after) === 0,
				`TypeScript reopen rewrote historical JSONL: ${relative(REPO_ROOT, file)}`,
			);
		}
		const entries = manager.getEntries();
		sourcePinnedManagerRetainsOpaqueFutureEntry ||= entries.some((entry) => entry.type === "future_thing");
		assert(manager.getHeader()?.version === 3, `TypeScript reopen did not see v3 header: ${file}`);
		assert(entries.length > 0, `TypeScript reopen lost entries: ${file}`);
		assert(manager.getTree().length > 0, `TypeScript reopen lost tree: ${file}`);
		assert(manager.buildSessionContext().messages.length > 0, `TypeScript reopen lost context: ${file}`);
		const leaf = manager.getLeafId();
		assert(
			leaf === finalEntry.id,
			`TypeScript reopen leaf differs from final entry: ${relative(REPO_ROOT, file)}`,
		);
	}
	assert(fixtureContainsOpaqueFutureEntry, "Rust output dropped the generated opaque future entry");
	assert(
		sourcePinnedManagerRetainsOpaqueFutureEntry,
		"source-pinned TypeScript SessionManager dropped the opaque future entry",
	);
	return files.length;
}

async function main(): Promise<void> {
	await rm(OUTPUT, { recursive: true, force: true });
	await run(["bun", "scripts/generate-session-fixtures.ts"]);
	await run(
		[
			"cargo",
			"test",
			"-p",
			"pi",
			"--lib",
			"generated_cross_version_session_interoperability",
			"--locked",
			"--",
			"--ignored",
		],
		{ PI_SESSION_INTEROP_OUTPUT: OUTPUT },
	);
	const reopened = await reopenWithSourcePinnedTypescript(OUTPUT);
	process.stdout.write(`Session interoperability passed; TypeScript reopened ${reopened} Rust-produced sessions.\n`);
}

if (import.meta.main) await main();
