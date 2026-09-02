import { expect, test } from "bun:test";
import { cp, mkdtemp, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import {
	SESSION_VERSION_UNKNOWN,
	reopenWithSourcePinnedTypescript,
	sessionVersionFromBytes,
} from "../verification/session-interop.ts";

const ROOT = resolve(import.meta.dir, "../..");
const FIXTURES = join(ROOT, "crates/pi/tests/fixtures/sessions");

test("source-pinned TypeScript pi reopens every generated fixture", async () => {
	const generated = Bun.spawnSync(["bun", "scripts/generate-session-fixtures.ts"], {
		cwd: ROOT,
		stdout: "pipe",
		stderr: "pipe",
	});
	if (generated.exitCode !== 0) {
		const stdout = generated.stdout?.toString().trim() ?? "";
		const stderr = generated.stderr?.toString().trim() ?? "";
		throw new Error(
			`generate-session-fixtures.ts exited ${generated.exitCode}\nstdout:\n${stdout}\nstderr:\n${stderr}`,
		);
	}
	expect(generated.exitCode).toBe(0);

	const directory = await mkdtemp(join(tmpdir(), "pi-session-interop-ts-"));
	try {
		await cp(FIXTURES, directory, { recursive: true });
		const expected = (await readdir(directory, { recursive: true })).filter(
			(path) => typeof path === "string" && path.endsWith(".jsonl"),
		).length;
		// The fixture set must be non-empty; otherwise a zero-count comparison
		// would pass vacuously and prove nothing about the reopen path.
		expect(expected).toBeGreaterThan(0);
		expect(await reopenWithSourcePinnedTypescript(directory)).toBe(expected);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
}, 120_000);

test("version classification separates legacy v1 headers from unclassifiable ones", () => {
	// A v1 session predates the `version` field, so its absence is the v1 signal.
	expect(sessionVersionFromBytes(Buffer.from('{"type":"session"}\n{"type":"message"}\n'))).toBe(1);
	expect(sessionVersionFromBytes(Buffer.from('{"version":3,"type":"session"}\n'))).toBe(3);
	// Unparseable or non-numeric headers must be rejected, never migrated silently.
	expect(sessionVersionFromBytes(Buffer.from("not json at all\n"))).toBe(SESSION_VERSION_UNKNOWN);
	expect(sessionVersionFromBytes(Buffer.from('{"version":"3"}\n'))).toBe(SESSION_VERSION_UNKNOWN);
});
