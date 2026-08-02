import { expect, test } from "bun:test";
import { cp, mkdtemp, readdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { reopenWithSourcePinnedTypescript } from "../verification/session-interop.ts";

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
	await cp(FIXTURES, directory, { recursive: true });
	const expected = (await readdir(directory, { recursive: true })).filter(
		(path) => typeof path === "string" && path.endsWith(".jsonl"),
	).length;
	expect(await reopenWithSourcePinnedTypescript(directory, { preserveHistoricalPrefix: false })).toBe(expected);
});
