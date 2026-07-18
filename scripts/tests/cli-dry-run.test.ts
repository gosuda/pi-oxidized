import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { decodeZipArchive, listTarGzEntries, sha256Bytes } from "../release/archive.ts";
import { SpawnRunner } from "../release/runner.ts";
import { archiveName as expectedArchiveName, planFor, RUST_TARGETS } from "../release/targets.ts";

const TARGET_TIMEOUT_MS = 20_000;

describe("package-release dry-run", () => {
	test("writes verified archives with every required member for all release targets", async () => {
		const runner = new SpawnRunner();
		const repoRoot = resolve(import.meta.dirname, "../..");
		const scriptPath = join(repoRoot, "scripts", "package-release.ts");
		const { version } = JSON.parse(readFileSync(join(repoRoot, "package.json"), "utf8")) as {
			version: string;
		};

		for (const target of RUST_TARGETS) {
			const plan = planFor(target);
			const work = mkdtempSync(join(tmpdir(), `pi-dryrun-${target}-`));
			try {
				const res = await runner.run(
					"bun",
					["run", scriptPath, "--target", target, "--dry-run", "--out", work],
					{ cwd: repoRoot, rejectOnError: false, timeoutMs: TARGET_TIMEOUT_MS },
				);
				if (res.exitCode !== 0) {
					throw new Error(`Dry-run failed for ${target}:\n${res.stdout}\n${res.stderr}`);
				}

				expect(res.stdout).toContain("=== Release Complete ===");
				const files = readdirSync(work);
				const archiveName = expectedArchiveName(version, plan);
				expect(files).toContain(archiveName);
				const checksumName = `${archiveName}.sha256`;
				expect(files).toContain(checksumName);
				const archivePath = join(work, archiveName);
				const checksumPath = join(work, checksumName);
				expect(statSync(archivePath).size).toBeGreaterThan(0);
				expect(statSync(checksumPath).size).toBeGreaterThan(0);

				const archiveBytes = new Uint8Array(readFileSync(archivePath));
				expect(readFileSync(checksumPath, "utf8")).toBe(
					`${sha256Bytes(archiveBytes)}  ${archiveName}\n`,
				);
				const members = plan.archive === "zip"
					? decodeZipArchive(archiveBytes).map((entry) => entry.path)
					: listTarGzEntries(archiveBytes);
				const prefix = `${plan.archiveDir}/`;
				expect(members).toContain(`${prefix}${plan.piBinaryName}`);
				expect(members).toContain(`${prefix}${plan.hostBinaryName}`);
				expect(members).toContain(`${prefix}release.json`);
			} finally {
				rmSync(work, { recursive: true, force: true });
			}
		}
	}, RUST_TARGETS.length * TARGET_TIMEOUT_MS + 5_000);
});
