import { describe, expect, test } from "bun:test";
import {
	chmodSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	readdirSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
	reconstructProviderData,
	type ProviderCatalog,
	type ReconstructProofContext,
	type ReconstructProviderDataResult,
} from "../reconstruct-provider-data.ts";
import { buildSortedCatalog, encodeCatalog } from "../generate-builtin-models.ts";

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const REAL_CATALOG_PATH = join(REPO_ROOT, "crates/pi-ai/data/builtin-models.json");
const REAL_PROVIDERS_DIR = join(
	REPO_ROOT,
	".references/pi/packages/ai/src/providers",
);
const REAL_DATA_DIR = join(REAL_PROVIDERS_DIR, "data");

type Fixture = {
	root: string;
	catalogPath: string;
	providersDir: string;
	dataDir: string;
};

function makeFixture(catalog: ProviderCatalog): Fixture {
	const root = mkdtempSync(join(tmpdir(), "reconstruct-provider-data-"));
	const providersDir = join(root, "providers");
	const dataDir = join(providersDir, "data");
	const catalogPath = join(root, "builtin-models.json");
	mkdirSync(providersDir, { recursive: true });
	writeFileSync(catalogPath, `${JSON.stringify(catalog, null, 2)}\n`, "utf8");
	for (const provider of Object.keys(catalog).sort()) {
		writeFileSync(join(providersDir, `${provider}.models.ts`), "// fixture wrapper\n", "utf8");
	}
	return { root, catalogPath, providersDir, dataDir };
}

function seedLiveData(dataDir: string, files: Record<string, string>): void {
	mkdirSync(dataDir, { recursive: true });
	for (const [name, body] of Object.entries(files)) {
		writeFileSync(join(dataDir, name), body, "utf8");
	}
}

function snapshotDir(dir: string): Map<string, string> | null {
	try {
		const names = readdirSync(dir).sort();
		const out = new Map<string, string>();
		for (const name of names) {
			out.set(name, readFileSync(join(dir, name), "utf8"));
		}
		return out;
	} catch {
		return null;
	}
}

function expectSnapshotsEqual(
	actual: Map<string, string> | null,
	expected: Map<string, string> | null,
): void {
	expect(actual === null).toBe(expected === null);
	if (actual === null || expected === null) return;
	expect([...actual.keys()]).toEqual([...expected.keys()]);
	for (const [name, body] of expected) {
		expect(actual.get(name)).toBe(body);
	}
}

function siblingArtifacts(providersDir: string, dataDirName = "data"): string[] {
	return readdirSync(providersDir)
		.filter(
			(name) =>
				name.startsWith(`${dataDirName}.staging.`) ||
				name.startsWith(`${dataDirName}.backup.`),
		)
		.sort();
}

async function noopProof(_ctx: ReconstructProofContext): Promise<void> {}

function asRecord(value: unknown): Record<string, unknown> {
	if (value === null || typeof value !== "object" || Array.isArray(value)) {
		throw new Error("expected JSON object");
	}
	return value as Record<string, unknown>;
}

describe("reconstructProviderData transaction (Cluster C)", () => {
	test("success publishes exact sorted provider JSON and leaves no staging/backup siblings", async () => {
		const catalog: ProviderCatalog = {
			beta: { "m-b": { id: "m-b", z: 1, a: 2 } },
			alpha: { "m-a": { id: "m-a", nested: { b: 1, a: 2 } } },
		};
		const fixture = makeFixture(catalog);
		try {
			const result = await reconstructProviderData({
				repoRoot: fixture.root,
				catalogPath: fixture.catalogPath,
				providersDir: fixture.providersDir,
				dataDir: fixture.dataDir,
				inversionProof: noopProof,
			});

			expect(result.written).toBe(2);
			expect(result.providers).toEqual(["alpha", "beta"]);
			expect(readdirSync(fixture.dataDir).sort()).toEqual(["alpha.json", "beta.json"]);
			expect(readFileSync(join(fixture.dataDir, "alpha.json"), "utf8")).toBe(
				`${JSON.stringify({ "m-a": { id: "m-a", nested: { a: 2, b: 1 } } }, null, "\t")}\n`,
			);
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("round-trips nested __proto__ model maps through reconstruction and inversion", async () => {
		const catalog = JSON.parse(
			'{"alpha":{"__proto__":{"id":"__proto__","nested":{"z":1,"__proto__":{"sentinel":true},"a":2}}}}',
		) as ProviderCatalog;
		const fixture = makeFixture(catalog);
		try {
			await reconstructProviderData({
				repoRoot: fixture.root,
				catalogPath: fixture.catalogPath,
				providersDir: fixture.providersDir,
				dataDir: fixture.dataDir,
				inversionProof: noopProof,
			});

			const providerModels = asRecord(
				JSON.parse(readFileSync(join(fixture.dataDir, "alpha.json"), "utf8")),
			);
			expect(Object.hasOwn(providerModels, "__proto__")).toBe(true);
			const reconstructedModel = asRecord(providerModels["__proto__"]);
			const reconstructedNested = asRecord(reconstructedModel.nested);
			expect(Object.hasOwn(reconstructedNested, "__proto__")).toBe(true);
			expect(reconstructedNested["__proto__"]).toEqual({ sentinel: true });

			const generatorInput = Object.create(null) as Record<
				string,
				Record<string, unknown>
			>;
			generatorInput.alpha = providerModels;
			const inverted = asRecord(
				JSON.parse(encodeCatalog(buildSortedCatalog(generatorInput))),
			);
			const invertedModels = asRecord(inverted.alpha);
			const invertedModel = asRecord(invertedModels["__proto__"]);
			const invertedNested = asRecord(invertedModel.nested);
			expect(Object.hasOwn(invertedModels, "__proto__")).toBe(true);
			expect(Object.hasOwn(invertedNested, "__proto__")).toBe(true);
			expect(invertedNested["__proto__"]).toEqual({ sentinel: true });
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("stale-provider-removal drops JSON for providers that no longer have wrappers", async () => {
		const catalog: ProviderCatalog = {
			keep: { model: { id: "model" } },
		};
		const fixture = makeFixture(catalog);
		try {
			seedLiveData(fixture.dataDir, {
				"keep.json": '{"stale":true}\n',
				"removed-provider.json": '{"orphan":true}\n',
				"notes.txt": "should disappear with directory swap\n",
			});

			await reconstructProviderData({
				repoRoot: fixture.root,
				catalogPath: fixture.catalogPath,
				providersDir: fixture.providersDir,
				dataDir: fixture.dataDir,
				inversionProof: noopProof,
			});

			expect(readdirSync(fixture.dataDir).sort()).toEqual(["keep.json"]);
			expect(readFileSync(join(fixture.dataDir, "keep.json"), "utf8")).toBe(
				`${JSON.stringify({ model: { id: "model" } }, null, "\t")}\n`,
			);
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("rollback/failure-injection restores live data byte-for-byte and removes artifacts", async () => {
		const catalog: ProviderCatalog = {
			alpha: { model: { id: "model", v: 1 } },
		};
		const fixture = makeFixture(catalog);
		try {
			seedLiveData(fixture.dataDir, {
				"alpha.json": '{"legacy":true}\n',
				"stale.json": '{"keep-me":true}\n',
			});
			const before = snapshotDir(fixture.dataDir);

			await expect(
				reconstructProviderData({
					repoRoot: fixture.root,
					catalogPath: fixture.catalogPath,
					providersDir: fixture.providersDir,
					dataDir: fixture.dataDir,
					inversionProof: async () => {
						throw new Error("injected inversion proof failure");
					},
				}),
			).rejects.toThrow("injected inversion proof failure");

			expectSnapshotsEqual(snapshotDir(fixture.dataDir), before);
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("concurrent same-directory reconstruction preserves original bytes through rollback", async () => {
		const catalog: ProviderCatalog = {
			alpha: { model: { id: "model", version: 2 } },
		};
		const fixture = makeFixture(catalog);
		const firstProofReached = Promise.withResolvers<void>();
		const releaseFirstProof = Promise.withResolvers<void>();
		seedLiveData(fixture.dataDir, {
			"alpha.json": '{"legacy":true}\n',
			"stale.json": '{"preserve":"these bytes"}\n',
		});
		const before = snapshotDir(fixture.dataDir);
		const first = reconstructProviderData({
			repoRoot: fixture.root,
			catalogPath: fixture.catalogPath,
			providersDir: fixture.providersDir,
			dataDir: fixture.dataDir,
			inversionProof: async () => {
				firstProofReached.resolve();
				await releaseFirstProof.promise;
				throw new Error("first transaction proof failure");
			},
		});
		let second: Promise<ReconstructProviderDataResult> | undefined;
		try {
			await firstProofReached.promise;
			let secondProofRan = false;
			second = reconstructProviderData({
				repoRoot: fixture.root,
				catalogPath: fixture.catalogPath,
				providersDir: fixture.providersDir,
				dataDir: fixture.dataDir,
				inversionProof: async () => {
					secondProofRan = true;
					const backup = siblingArtifacts(fixture.providersDir).find((name) =>
						name.startsWith("data.backup."),
					);
					if (backup === undefined) {
						throw new Error("second transaction did not retain a backup");
					}
					expectSnapshotsEqual(snapshotDir(join(fixture.providersDir, backup)), before);
				},
			});

			// This integration check needs the competing filesystem operation to run;
			// fake timers cannot advance the OS-backed mkdir contention.
			await Bun.sleep(25);
			const secondProofRanWhileFirstHeld = secondProofRan;
			releaseFirstProof.resolve();
			const [firstResult, secondResult] = await Promise.allSettled([first, second]);

			expect(secondProofRanWhileFirstHeld).toBe(false);
			expect(firstResult.status).toBe("rejected");
			if (firstResult.status === "rejected") {
				expect(String(firstResult.reason)).toContain("first transaction proof failure");
			}
			expect(secondResult.status).toBe("fulfilled");
			expectSnapshotsEqual(snapshotDir(fixture.dataDir), new Map([
				[
					"alpha.json",
					`${JSON.stringify({ model: { id: "model", version: 2 } }, null, "\t")}\n`,
				],
			]));
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			releaseFirstProof.resolve();
			await first.catch(() => undefined);
			if (second !== undefined) {
				await second.catch(() => undefined);
			}
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("absent-live-dir publishes a fresh data directory", async () => {
		const catalog: ProviderCatalog = {
			solo: { only: { id: "only" } },
		};
		const fixture = makeFixture(catalog);
		try {
			expect(snapshotDir(fixture.dataDir)).toBeNull();

			await reconstructProviderData({
				repoRoot: fixture.root,
				catalogPath: fixture.catalogPath,
				providersDir: fixture.providersDir,
				dataDir: fixture.dataDir,
				inversionProof: noopProof,
			});

			expect(readdirSync(fixture.dataDir)).toEqual(["solo.json"]);
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("post-proof backup cleanup failure keeps published live tree and surfaces error", async () => {
		const catalog: ProviderCatalog = {
			alpha: { model: { id: "model", v: 2 } },
		};
		const fixture = makeFixture(catalog);
		try {
			seedLiveData(fixture.dataDir, {
				"alpha.json": '{"legacy":true}\n',
				"stale.json": '{"keep-me":true}\n',
			});
			const before = snapshotDir(fixture.dataDir);

			await expect(
				reconstructProviderData({
					repoRoot: fixture.root,
					catalogPath: fixture.catalogPath,
					providersDir: fixture.providersDir,
					dataDir: fixture.dataDir,
					inversionProof: noopProof,
					removeBackup: async () => {
						throw new Error("injected backup cleanup failure");
					},
				}),
			).rejects.toThrow(
				/reconstruction published successfully but failed to remove backup .*injected backup cleanup failure/,
			);

			const after = snapshotDir(fixture.dataDir);
			expect(after).not.toBeNull();
			// Commit point already passed: live tree must stay on the new publish,
			// not roll back to the stale pre-publish snapshot.
			expect([...after!.keys()]).toEqual(["alpha.json"]);
			expect(after!.get("alpha.json")).toBe(
				`${JSON.stringify({ model: { id: "model", v: 2 } }, null, "\t")}\n`,
			);
			expect(after!.get("stale.json")).toBeUndefined();
			expect(before!.has("stale.json")).toBe(true);
			const leftovers = siblingArtifacts(fixture.providersDir);
			expect(leftovers.some((name) => name.includes(".backup."))).toBe(true);
			expect(leftovers.some((name) => name.includes(".staging."))).toBe(false);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("absent-live-dir rolls back to absent when the inversion proof fails", async () => {
		const catalog: ProviderCatalog = {
			solo: { only: { id: "only" } },
		};
		const fixture = makeFixture(catalog);
		try {
			await expect(
				reconstructProviderData({
					repoRoot: fixture.root,
					catalogPath: fixture.catalogPath,
					providersDir: fixture.providersDir,
					dataDir: fixture.dataDir,
					inversionProof: async () => {
						throw new Error("injected proof failure on first publish");
					},
				}),
			).rejects.toThrow("injected proof failure on first publish");

			expect(snapshotDir(fixture.dataDir)).toBeNull();
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("repeat-run is content-idempotent against the real checkout tree", async () => {
		const catalogBefore = readFileSync(REAL_CATALOG_PATH);
		const first = await reconstructProviderData({
			repoRoot: REPO_ROOT,
			catalogPath: REAL_CATALOG_PATH,
			providersDir: REAL_PROVIDERS_DIR,
			dataDir: REAL_DATA_DIR,
		});
		const afterFirst = snapshotDir(REAL_DATA_DIR);
		expect(afterFirst).not.toBeNull();
		const expectedNames = first.providers.map((id) => `${id}.json`).sort();
		expect([...afterFirst!.keys()].sort()).toEqual(expectedNames);

		const second = await reconstructProviderData({
			repoRoot: REPO_ROOT,
			catalogPath: REAL_CATALOG_PATH,
			providersDir: REAL_PROVIDERS_DIR,
			dataDir: REAL_DATA_DIR,
		});
		const afterSecond = snapshotDir(REAL_DATA_DIR);

		expect(second.providers).toEqual(first.providers);
		expectSnapshotsEqual(afterSecond, afterFirst);
		expect(Buffer.compare(readFileSync(REAL_CATALOG_PATH), catalogBefore)).toBe(0);
		expect(siblingArtifacts(REAL_PROVIDERS_DIR)).toEqual([]);
	}, 120_000);
});

describe("reconstructProviderData default inversion proof path gating (P2)", () => {
	test("custom paths without an explicit inversionProof fail fast with a clear error", async () => {
		const catalog: ProviderCatalog = {
			alpha: { model: { id: "model" } },
		};
		const fixture = makeFixture(catalog);
		try {
			await expect(
				reconstructProviderData({
					repoRoot: fixture.root,
					catalogPath: fixture.catalogPath,
					providersDir: fixture.providersDir,
					dataDir: fixture.dataDir,
				}),
			).rejects.toThrow(
				/default inversion proof only covers repository default paths.*explicit inversionProof.*custom paths/,
			);
			// Fail-fast: no publish, no leftover siblings.
			expect(snapshotDir(fixture.dataDir)).toBeNull();
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("custom paths with an explicit inversionProof run that proof", async () => {
		const catalog: ProviderCatalog = {
			alpha: { model: { id: "model", v: 1 } },
		};
		const fixture = makeFixture(catalog);
		const seen: ReconstructProofContext[] = [];
		try {
			const result = await reconstructProviderData({
				repoRoot: fixture.root,
				catalogPath: fixture.catalogPath,
				providersDir: fixture.providersDir,
				dataDir: fixture.dataDir,
				inversionProof: async (ctx) => {
					seen.push(ctx);
				},
			});

			expect(result.written).toBe(1);
			expect(seen).toHaveLength(1);
			expect(seen[0]?.catalogPath).toBe(fixture.catalogPath);
			expect(seen[0]?.dataDir).toBe(fixture.dataDir);
			expect(readdirSync(fixture.dataDir)).toEqual(["alpha.json"]);
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("default-path reconstruction uses the current Bun executable for inversion proof", async () => {
		const catalogBefore = readFileSync(REAL_CATALOG_PATH);
		const fakeBin = mkdtempSync(join(tmpdir(), "reconstruct-provider-data-fake-bun-"));
		const fakeBun = join(fakeBin, "bun");
		writeFileSync(fakeBun, "#!/bin/sh\nexit 97\n", "utf8");
		chmodSync(fakeBun, 0o755);
		const previousPath = process.env.PATH;
		try {
			process.env.PATH = fakeBin;
			const result = await reconstructProviderData({
				repoRoot: REPO_ROOT,
				catalogPath: REAL_CATALOG_PATH,
				providersDir: REAL_PROVIDERS_DIR,
				dataDir: REAL_DATA_DIR,
				// inversionProof intentionally omitted — default proof must apply.
			});
			expect(result.written).toBeGreaterThan(0);
			expect(result.providers.length).toBe(result.written);
			expect(Buffer.compare(readFileSync(REAL_CATALOG_PATH), catalogBefore)).toBe(0);
			expect(siblingArtifacts(REAL_PROVIDERS_DIR)).toEqual([]);
		} finally {
			if (previousPath === undefined) {
				delete process.env.PATH;
			} else {
				process.env.PATH = previousPath;
			}
			rmSync(fakeBin, { recursive: true, force: true });
		}
	}, 120_000);
});
