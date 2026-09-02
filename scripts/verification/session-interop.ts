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
import { assertCanonicalReference, canonicalReferenceRoot } from "../reference-identity.ts";

const REPO_ROOT = resolve(import.meta.dirname, "../..");
const FIXTURES = join(
	REPO_ROOT,
	"crates/pi/tests/fixtures/sessions",
);
const OUTPUT = join(REPO_ROOT, "target/verification/session-interop");
// Canonical pi-2.0 module map — what canonical session-manager.ts actually pulls in:
// AgentMessage (type-only) from packages/agent/src/types.ts, uuidv7 from packages/ai/src/utils/uuid.ts.
const REFERENCE_ROOT = canonicalReferenceRoot(REPO_ROOT);
const REFERENCE_AGENT_TYPES = join(REFERENCE_ROOT, "packages/agent/src/types.ts");
const REFERENCE_AI_UUID = join(REFERENCE_ROOT, "packages/ai/src/utils/uuid.ts");

interface SessionManagerLike {
	getHeader(): Record<string, unknown> | null;
	getEntries(): readonly Record<string, unknown>[];
	getTree(): readonly unknown[];
	buildSessionContext(): { messages: readonly unknown[] };
	getLeafId(): string | null;
	getSessionName(): string | undefined;
}

interface SessionManagerStatic {
	open(path: string): SessionManagerLike;
}

function assert(condition: unknown, message: string): asserts condition {
	if (!condition) throw new Error(message);
}

/** Sentinel for a session whose first record is unparseable or carries a non-numeric version. */
export const SESSION_VERSION_UNKNOWN = -1;

/** A v1 session header predates the `version` field, so its absence IS the v1 signal. */
const SESSION_VERSION_LEGACY = 1;

/**
 * Detect the session version from the first JSONL line. Returns
 * `SESSION_VERSION_UNKNOWN` when the first record is unparseable or carries a
 * present-but-non-numeric version, so the caller rejects an unclassifiable file
 * instead of silently taking the lenient migration branch. A parseable header
 * with no `version` field is a genuine v1 session, not an unreadable one.
 */
export function sessionVersionFromBytes(bytes: Buffer): number {
	const newlineIndex = bytes.indexOf(0x0a);
	const firstLine = newlineIndex >= 0 ? bytes.subarray(0, newlineIndex) : bytes;
	let header: { version?: unknown };
	try {
		header = JSON.parse(firstLine.toString("utf8")) as { version?: unknown };
	} catch {
		return SESSION_VERSION_UNKNOWN;
	}
	if (header.version === undefined) return SESSION_VERSION_LEGACY;
	return typeof header.version === "number" ? header.version : SESSION_VERSION_UNKNOWN;
}

function installReferenceResolver(): void {
	Bun.plugin({
		name: "source-pinned-pi-reference",
		setup(build) {
			build.onResolve({ filter: /^@earendil-works\/pi-agent-core$/ }, () => ({
				path: REFERENCE_AGENT_TYPES,
			}));
			build.onResolve({ filter: /^@earendil-works\/pi-ai$/ }, () => ({
				path: REFERENCE_AI_UUID,
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
	assertCanonicalReference(REPO_ROOT);
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
): Promise<number> {
	const SessionManager = await referenceSessionManager();
	const files = await jsonlFiles(root);
	assert(files.length > 0, `Rust proof did not produce session files under ${root}`);
	let preservedUnknownEntry = false;
	const OPAQUE_ENTRY_MARKER = Buffer.from('"type":"future_thing"');

	for (const file of files) {
		const before = await readFile(file);
		const versionBefore = sessionVersionFromBytes(before);
		assert(
			versionBefore !== SESSION_VERSION_UNKNOWN,
			`session file has an unparseable or non-numeric version header: ${relative(REPO_ROOT, file)}`,
		);
		const manager = SessionManager.open(file);
		const after = await readFile(file);
		// Verify the opaque entry survived reopen by asking the manager's
		// parsed in-memory view — the only independent evidence that the
		// TypeScript SessionManager retained the entry, not just that the
		// bytes on disk still contain it.
		const reopenedEntries = manager.getEntries();
		const hasOpaqueEntry = reopenedEntries.some(
			(entry) => entry.type === "future_thing",
		);
		if (before.includes(OPAQUE_ENTRY_MARKER)) {
			assert(
				hasOpaqueEntry,
				`TypeScript reopen dropped the opaque future_thing entry: ${relative(REPO_ROOT, file)}`,
			);
			preservedUnknownEntry = true;
		}
		if (versionBefore >= 3) {
			// Already-current sessions must not be rewritten on reopen.
			assert(
				Buffer.compare(before, after) === 0,
				`TypeScript reopen rewrote historical JSONL: ${relative(REPO_ROOT, file)}`,
			);
	} else {
		// v1/v2 sessions are migrated to v3 on reopen; verify the migration
		// produced a v3 header before checking that opaque entries survived.
		assert(
			sessionVersionFromBytes(after) === 3,
			`TypeScript reopen did not migrate to v3: ${relative(REPO_ROOT, file)}`,
		);
		if (before.includes(OPAQUE_ENTRY_MARKER)) {
			assert(
				after.includes(OPAQUE_ENTRY_MARKER),
				`TypeScript reopen dropped opaque future_thing entry during migration: ${relative(REPO_ROOT, file)}`,
			);
		}
	}
		assert(manager.getHeader()?.version === 3, `TypeScript reopen did not see v3 header: ${file}`);
		assert(manager.getEntries().length > 0, `TypeScript reopen lost entries: ${file}`);
		assert(manager.getTree().length > 0, `TypeScript reopen lost tree: ${file}`);
		assert(manager.buildSessionContext().messages.length > 0, `TypeScript reopen lost context: ${file}`);
		assert(manager.getLeafId() !== null, `TypeScript reopen lost leaf: ${file}`);
	}
	assert(preservedUnknownEntry, "TypeScript reopen dropped the opaque future_thing entry");
	return files.length;
}

async function main(): Promise<void> {
	await rm(OUTPUT, { recursive: true, force: true });
	await run(["bun", "scripts/generate-session-fixtures.ts"]);
	await run(
		["cargo", "test", "-p", "pi", "--lib", "generated_cross_version_session_interoperability", "--locked"],
		{ PI_SESSION_INTEROP_OUTPUT: OUTPUT },
	);
	const reopened = await reopenWithSourcePinnedTypescript(OUTPUT);
	process.stdout.write(`Session interoperability passed; TypeScript reopened ${reopened} Rust-produced sessions.\n`);
}

if (import.meta.main) await main();
