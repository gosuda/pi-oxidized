import { expect, test } from "bun:test";
import { cp, mkdtemp, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import {
	FIXTURE_GENERATION_ARGS,
	reopenWithSourcePinnedTypescript,
	SESSION_INTEROP_TIMEOUT_MS,
} from "../verification/session-interop.ts";

const ROOT = resolve(import.meta.dir, "../..");
const FIXTURES = join(ROOT, ".agent-tasks/pi-rust-rewrite/fixtures/sessions");

test("uses the product matrix timeout for source-pinned session interoperability", async () => {
	const matrix = (await Bun.file(join(ROOT, "scripts/verification/compat-matrix.json")).json()) as {
		readonly rows: readonly {
			readonly id: string;
			readonly commands: readonly { readonly timeoutMs?: number }[];
		}[];
	};
	const command = matrix.rows.find((row) => row.id === "session-v1-v2-v3-interop")?.commands[0];
	expect(command?.timeoutMs).toBe(SESSION_INTEROP_TIMEOUT_MS);
});

test("fixture generation uses the current Bun runtime via process.execPath, not a PATH-resolved bun", () => {
	expect(FIXTURE_GENERATION_ARGS[0]).toBe(process.execPath);
	expect(FIXTURE_GENERATION_ARGS[0]).not.toBe("bun");
	expect(FIXTURE_GENERATION_ARGS[1]).toBe("scripts/generate-session-fixtures.ts");
});

test("source-pinned TypeScript pi reopens every generated fixture", async () => {
	const generated = Bun.spawnSync([process.execPath, "scripts/generate-session-fixtures.ts"], {
		cwd: ROOT,
		stdout: "pipe",
		stderr: "pipe",
		timeout: SESSION_INTEROP_TIMEOUT_MS,
	});
	expect(generated.exitCode).toBe(0);

	const expected = (await readdir(FIXTURES, { recursive: true })).filter(
		(path) => typeof path === "string" && path.endsWith(".jsonl"),
	).length;
	expect(expected).toBeGreaterThan(0);

	const directory = await mkdtemp(join(tmpdir(), "pi-session-interop-ts-"));
	try {
		await cp(FIXTURES, directory, { recursive: true });
		const actual = await reopenWithSourcePinnedTypescript(directory, {
			preserveHistoricalPrefix: false,
		});
		expect(actual).toBe(expected);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
}, SESSION_INTEROP_TIMEOUT_MS);
