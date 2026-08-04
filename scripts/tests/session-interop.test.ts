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
const FIXTURES = join(ROOT, ".agent-tasks/pi-rust-rewrite/fixtures/sessions");

test("source-pinned TypeScript pi reopens every generated fixture", async () => {
	const generated = Bun.spawnSync(["bun", "scripts/generate-session-fixtures.ts"], {
		cwd: ROOT,
		stdout: "pipe",
		stderr: "pipe",
	});
	expect(generated.exitCode).toBe(0);

	const directory = await mkdtemp(join(tmpdir(), "pi-session-interop-ts-"));
	try {
		await cp(FIXTURES, directory, { recursive: true });
		const expected = (await readdir(directory, { recursive: true })).filter(
			(path) => typeof path === "string" && path.endsWith(".jsonl"),
		).length;
		expect(await reopenWithSourcePinnedTypescript(directory)).toBe(expected);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test("version classification separates legacy v1 headers from unclassifiable ones", () => {
	// A v1 session predates the `version` field, so its absence is the v1 signal.
	expect(sessionVersionFromBytes(Buffer.from('{"type":"session"}\n{"type":"message"}\n'))).toBe(1);
	expect(sessionVersionFromBytes(Buffer.from('{"version":3,"type":"session"}\n'))).toBe(3);
	// Unparseable or non-numeric headers must be rejected, never migrated silently.
	expect(sessionVersionFromBytes(Buffer.from("not json at all\n"))).toBe(SESSION_VERSION_UNKNOWN);
	expect(sessionVersionFromBytes(Buffer.from('{"version":"3"}\n'))).toBe(SESSION_VERSION_UNKNOWN);
});
