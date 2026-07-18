import { describe, expect, test } from "bun:test";
import { SpawnRunner } from "../release/runner.ts";
import { RUST_TARGETS } from "../release/targets.ts";
import { join, resolve } from "node:path";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";

describe("package-release dry-run", () => {
	// 5 targets. The script runs fast on dry run, but Bun testing them all 
	// sequentially might take a few seconds. We increase timeout.
	test("successfully dry-runs all 5 Rust targets without error", async () => {
		const runner = new SpawnRunner();
		const repoRoot = resolve(import.meta.dirname, "../..");
		const scriptPath = join(repoRoot, "scripts", "package-release.ts");

		for (const target of RUST_TARGETS) {
			const work = mkdtempSync(join(tmpdir(), `pi-dryrun-${target}-`));
			try {
				const res = await runner.run(
					"bun",
					["run", scriptPath, "--target", target, "--dry-run", "--out", work],
					{ cwd: repoRoot, rejectOnError: false },
				);

				if (res.exitCode !== 0) {
					throw new Error(`Dry-run failed for ${target}:\n${res.stdout}\n${res.stderr}`);
				}

				expect(res.exitCode).toBe(0);
				expect(res.stdout).toContain(`=== Release Complete ===`);
				expect(res.stdout).toContain(`Archive:  ${work}/pi-`);
			} finally {
				rmSync(work, { recursive: true, force: true });
			}
		}
	}, 30000); // 30s timeout should be plenty
});
