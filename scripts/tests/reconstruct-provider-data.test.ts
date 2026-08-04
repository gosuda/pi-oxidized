import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import {
	chmodSync,
	cpSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	readdirSync,
	rmSync,
	utimesSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
	acquireDataDirectoryLock,
	defaultInversionProof,
	recoverStaleLock,
	reconstructProviderData,
	releaseDataDirectoryLock,
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
const REFERENCE_PROVIDERS_AVAILABLE = (() => {
	try {
		readdirSync(REAL_PROVIDERS_DIR);
		return true;
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
		throw error;
	}
})();

/**
 * Copy the real provider-data tree into an isolated temp fixture so
 * reconstruction proofs never mutate the reference checkout. Returns null
 * when the reference providers directory is not provisioned, so callers can
 * skip the test rather than writing to the live checkout.
 */
function realProvidersFixture(): {
	root: string;
	catalogPath: string;
	providersDir: string;
	dataDir: string;
} | null {
	try {
		readdirSync(REAL_PROVIDERS_DIR);
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
		throw error;
	}
	const root = mkdtempSync(join(tmpdir(), "reconstruct-real-copy-"));
	const providersDir = join(root, "providers");
	const dataDir = join(providersDir, "data");
	const catalogPath = join(root, "builtin-models.json");
	cpSync(REAL_PROVIDERS_DIR, providersDir, { recursive: true });
	cpSync(REAL_CATALOG_PATH, catalogPath);
	return { root, catalogPath, providersDir, dataDir };
}

type Fixture = {
	root: string;
	catalogPath: string;
	providersDir: string;
	dataDir: string;
};

function withFixtureApis(catalog: ProviderCatalog): ProviderCatalog {
	const normalized = Object.create(null) as ProviderCatalog;
	for (const [provider, models] of Object.entries(catalog)) {
		const providerModels = Object.create(null) as Record<string, unknown>;
		for (const [modelId, value] of Object.entries(models)) {
			const model = asRecord(value);
			providerModels[modelId] = Object.assign(Object.create(null), model, {
				api: typeof model.api === "string" ? model.api : "test-api",
			});
		}
		normalized[provider] = providerModels;
	}
	return normalized;
}

function makeFixture(catalog: ProviderCatalog): Fixture {
	const root = mkdtempSync(join(tmpdir(), "reconstruct-provider-data-"));
	const providersDir = join(root, "providers");
	const dataDir = join(providersDir, "data");
	const catalogPath = join(root, "builtin-models.json");
	const fixtureCatalog = withFixtureApis(catalog);
	mkdirSync(providersDir, { recursive: true });
	writeFileSync(catalogPath, `${JSON.stringify(fixtureCatalog, null, 2)}\n`, "utf8");
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
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
		throw error;
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
				`${JSON.stringify({ "test-api": { "m-a": { api: "test-api", id: "m-a", nested: { a: 2, b: 1 } } } }, null, "\t")}\n`,
			);
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("rebuilds provider manifest hashes while preserving its generation timestamp", async () => {
		const catalog: ProviderCatalog = {
			alpha: { model: { api: "test-api", id: "model" } },
		};
		const fixture = makeFixture(catalog);
		const generatedAt = "2026-08-03T00:00:00.000Z";
		const staleManifest = `${JSON.stringify({
			schemaVersion: 3,
			generatedAt,
			structureHash: "stale",
			files: { "alpha.json": "stale" },
		})}\n`;
		try {
			seedLiveData(fixture.dataDir, {
				".manifest.json": staleManifest,
				"alpha.json": '{"stale":true}\n',
			});
			await reconstructProviderData({
				repoRoot: fixture.root,
				catalogPath: fixture.catalogPath,
				providersDir: fixture.providersDir,
				dataDir: fixture.dataDir,
				inversionProof: noopProof,
			});

			const providerBody = readFileSync(join(fixture.dataDir, "alpha.json"), "utf8");
			const manifest = asRecord(
				JSON.parse(readFileSync(join(fixture.dataDir, ".manifest.json"), "utf8")),
			);
			const files = asRecord(manifest.files);
			expect(manifest.generatedAt).toBe(generatedAt);
			expect(manifest.structureHash).toBe(
				createHash("sha256")
					.update(JSON.stringify({ alpha: { model: "test-api" } }))
					.digest("hex"),
			);
			expect(files["alpha.json"]).toBe(
				createHash("sha256").update(providerBody).digest("hex"),
			);
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

			const providerGroups = asRecord(
				JSON.parse(readFileSync(join(fixture.dataDir, "alpha.json"), "utf8")),
			);
			const providerModels = asRecord(providerGroups["test-api"]);
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
				`${JSON.stringify({ "test-api": { model: { api: "test-api", id: "model" } } }, null, "\t")}\n`,
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
	test("failed backup rename reports only the primary error and keeps live data", async () => {
		const catalog: ProviderCatalog = {
			alpha: { model: { id: "model", v: 1 } },
		};
		const fixture = makeFixture(catalog);
		try {
			seedLiveData(fixture.dataDir, {
				"alpha.json": "{\"legacy\":true}\n",
				"stale.json": "{\"keep-me\":true}\n",
			});
			const before = snapshotDir(fixture.dataDir);

			const error = await captureError(
				reconstructProviderData({
					repoRoot: fixture.root,
					catalogPath: fixture.catalogPath,
					providersDir: fixture.providersDir,
					dataDir: fixture.dataDir,
					inversionProof: noopProof,
					backupLive: async () => {
						throw new Error("injected backup rename failure");
					},
				}),
			);

			expect(error.message).toBe(
				"failed to rename live data to backup: injected backup rename failure",
			);
			expectSnapshotsEqual(snapshotDir(fixture.dataDir), before);
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});
	test("blocked publish rollback preserves the known-good backup and reports its path", async () => {
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

			const error = await captureError(
				reconstructProviderData({
					repoRoot: fixture.root,
					catalogPath: fixture.catalogPath,
					providersDir: fixture.providersDir,
					dataDir: fixture.dataDir,
					inversionProof: noopProof,
					publishStaging: async (_stagingDir, dataDir) => {
						mkdirSync(dataDir, { recursive: true });
						throw new Error("injected publish failure after unexpected live path");
					},
				}),
			);

			const primaryMessage =
				"failed to publish staging directory to live data: injected publish failure after unexpected live path";
			const primaryOffset = error.message.indexOf(primaryMessage);
			const restoreOffset = error.message.indexOf("additionally failed to restore live data");
			expect(primaryOffset).toBeGreaterThanOrEqual(0);
			expect(restoreOffset).toBeGreaterThan(primaryOffset);
			const backupName = siblingArtifacts(fixture.providersDir).find((name) =>
				name.startsWith("data.backup."),
			);
			if (backupName === undefined) throw new Error("expected preserved backup after blocked restore");
			const backupPath = join(fixture.providersDir, backupName);
			expect(error.message).toContain(backupPath);
			expectSnapshotsEqual(snapshotDir(backupPath), before);
			expect(snapshotDir(fixture.dataDir)).toEqual(new Map());
			expect(siblingArtifacts(fixture.providersDir)).toEqual([backupName]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});
	test("lock release failure retains the primary reconstruction failure first", async () => {
		const catalog: ProviderCatalog = {
			alpha: { model: { id: "model", v: 1 } },
		};
		const fixture = makeFixture(catalog);
		const primaryError = new Error("injected primary reconstruction failure");
		try {
			const error = await captureError(
				reconstructProviderData({
					repoRoot: fixture.root,
					catalogPath: fixture.catalogPath,
					providersDir: fixture.providersDir,
					dataDir: fixture.dataDir,
					inversionProof: async () => {
						throw primaryError;
					},
					releaseLock: async () => {
						throw new Error("injected lock release failure");
					},
				}),
			);

			const primaryOffset = error.message.indexOf("injected primary reconstruction failure");
			const releaseOffset = error.message.indexOf("injected lock release failure");
			expect(primaryOffset).toBeGreaterThanOrEqual(0);
			expect(releaseOffset).toBeGreaterThan(primaryOffset);
			expect(error.cause).toBe(primaryError);
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
			// Deferred barrier: resolves only when the second contender's
			// onLockWait fires, proving it observed the first owner's held
			// lock before that owner releases. No wall-clock sleep can
			// degrade this to sequential execution.
			const secondObservedHeldLock = Promise.withResolvers<void>();
			second = reconstructProviderData({
				repoRoot: fixture.root,
				catalogPath: fixture.catalogPath,
				providersDir: fixture.providersDir,
				dataDir: fixture.dataDir,
				onLockWait: () => {
					secondObservedHeldLock.resolve();
				},
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

			// Wait until the second contender is proven to have observed the
			// held lock; at that moment its proof must not yet have entered.
			await secondObservedHeldLock.promise;
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
					`${JSON.stringify({ "test-api": { model: { api: "test-api", id: "model", version: 2 } } }, null, "\t")}\n`,
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
				`${JSON.stringify({ "test-api": { model: { api: "test-api", id: "model", v: 2 } } }, null, "\t")}\n`,
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

	test.skipIf(!REFERENCE_PROVIDERS_AVAILABLE)("repeat-run is content-idempotent against a temp copy of the real tree", async () => {
		const fx = realProvidersFixture();
		if (fx === null) throw new Error("reference providers must be provisioned (guarded by test.skipIf)");
		const catalogBefore = readFileSync(fx.catalogPath);
		const dataBefore = snapshotDir(fx.dataDir);
		try {
			const first = await reconstructProviderData({
				repoRoot: fx.root,
				catalogPath: fx.catalogPath,
				providersDir: fx.providersDir,
				dataDir: fx.dataDir,
				inversionProof: noopProof,
			});
			const afterFirst = snapshotDir(fx.dataDir);
			if (afterFirst === null) throw new Error("expected reconstructed provider data");
			const expectedNames = first.providers.map((id) => `${id}.json`);
			if (dataBefore?.has(".manifest.json") === true) expectedNames.push(".manifest.json");
			expectedNames.sort();
			expect([...afterFirst.keys()].sort()).toEqual(expectedNames);
			if (dataBefore?.has(".manifest.json") === true) {
				const beforeManifest = asRecord(JSON.parse(dataBefore.get(".manifest.json") ?? ""));
				const afterManifest = asRecord(JSON.parse(afterFirst.get(".manifest.json") ?? ""));
				expect(afterManifest.generatedAt).toBe(beforeManifest.generatedAt);
			}

			const second = await reconstructProviderData({
				repoRoot: fx.root,
				catalogPath: fx.catalogPath,
				providersDir: fx.providersDir,
				dataDir: fx.dataDir,
				inversionProof: noopProof,
			});
			const afterSecond = snapshotDir(fx.dataDir);

			expect(second.providers).toEqual(first.providers);
			expectSnapshotsEqual(afterSecond, afterFirst);
			expect(Buffer.compare(readFileSync(fx.catalogPath), catalogBefore)).toBe(0);
			expect(siblingArtifacts(fx.providersDir)).toEqual([]);
		} finally {
			rmSync(fx.root, { recursive: true, force: true });
		}
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

	test("default inversion proof uses the current Bun executable, not PATH", async () => {
		const root = mkdtempSync(join(tmpdir(), "reconstruct-path-decoy-"));
		const scriptsDir = join(root, "scripts");
		const catalogPath = join(root, "builtin-models.json");
		mkdirSync(scriptsDir, { recursive: true });
		writeFileSync(catalogPath, '{"alpha":{"model":{"id":"model"}}}\n', "utf8");
		// Fixture generator: exits 0 so the proof succeeds when process.execPath is used.
		writeFileSync(
			join(scriptsDir, "generate-builtin-models.ts"),
			"// Fixture generator: exits cleanly so the inversion proof succeeds.\nprocess.exit(0);\n",
			"utf8",
		);
		const fakeBin = mkdtempSync(join(tmpdir(), "reconstruct-provider-data-fake-bun-"));
		const fakeBun = join(fakeBin, "bun");
		writeFileSync(fakeBun, "#!/bin/sh\nexit 97\n", "utf8");
		chmodSync(fakeBun, 0o755);
		const previousPath = process.env.PATH;
		try {
			process.env.PATH = fakeBin;
			// If the proof used PATH `bun`, the fake would exit 97 and the proof
			// would fail. process.execPath is absolute, so the real Bun runs the
			// fixture generator (exit 0) and the proof passes.
			await defaultInversionProof({
				repoRoot: root,
				catalogPath,
				providersDir: join(root, "providers"),
				dataDir: join(root, "providers", "data"),
			});
		} finally {
			if (previousPath === undefined) {
				delete process.env.PATH;
			} else {
				process.env.PATH = previousPath;
			}
			rmSync(fakeBin, { recursive: true, force: true });
			rmSync(root, { recursive: true, force: true });
		}
	}, 120_000);
});

describe("defaultInversionProof primary-error preservation (PRRT_kwDOTcPStM6UYj9z)", () => {
	test("restore failure after proof failure preserves the proof failure as primary", async () => {
		const root = mkdtempSync(join(tmpdir(), "inversion-proof-preserve-"));
		const catalogPath = join(root, "builtin-models.json");
		writeFileSync(catalogPath, '{"alpha":{"model":{"id":"model"}}}\n', "utf8");
		try {
			const error = await captureError(
				defaultInversionProof({
					// repoRoot without scripts/generate-builtin-models.ts → proof fails.
					repoRoot: root,
					catalogPath,
					providersDir: join(root, "providers"),
					dataDir: join(root, "providers", "data"),
					// Inject restore failure through the API so the test is
					// deterministic regardless of uid (chmod 0o444 does not stop root).
					restoreCatalog: async () => {
						throw new Error("injected restore failure");
					},
				}),
			);

			const proofOffset = error.message.indexOf("inversion proof failed");
			const restoreOffset = error.message.indexOf(
				"additionally failed to restore catalog snapshot",
			);
			// Both messages present.
			expect(proofOffset).toBeGreaterThanOrEqual(0);
			expect(restoreOffset).toBeGreaterThan(proofOffset);
			// Causal order: proof failure is the cause, not the restore failure.
			expect(error.cause).toBeInstanceOf(Error);
			expect((error.cause as Error).message).toContain("inversion proof failed");
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});

	test("restore failure without proof failure surfaces the restore error directly", async () => {
		const root = mkdtempSync(join(tmpdir(), "inversion-proof-restore-only-"));
		const providersDir = join(root, "providers");
		const dataDir = join(providersDir, "data");
		const catalogPath = join(root, "crates", "pi-ai", "data", "builtin-models.json");
		const generatorPath = join(root, "scripts", "generate-builtin-models.ts");

		mkdirSync(dirname(catalogPath), { recursive: true });
		mkdirSync(dirname(generatorPath), { recursive: true });
		writeFileSync(catalogPath, '{"alpha":{"model":{"id":"model"}}}\n', "utf8");
		// No-op fixture generator: exits cleanly so the inversion proof succeeds.
		writeFileSync(
			generatorPath,
			"// Fixture generator: does nothing so the inversion proof succeeds.\nprocess.exit(0);\n",
			"utf8",
		);

		try {
			const error = await captureError(
				defaultInversionProof({
					repoRoot: root,
					catalogPath,
					providersDir,
					dataDir,
					// Inject restore failure through the API so the test is
					// deterministic regardless of uid (chmod 0o444 does not stop root).
					restoreCatalog: async () => {
						throw new Error("injected restore failure");
					},
				}),
			);

			// No "inversion proof failed" wrapper — the raw restore error surfaces.
			expect(error.message).not.toContain("inversion proof failed");
			expect(error.message).toContain("injected restore failure");
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});
});

const LOCK_OWNER_FILE = "owner.json";

type LockFixture = { root: string; dataDir: string; lockDir: string };

function makeLockFixture(): LockFixture {
	const root = mkdtempSync(join(tmpdir(), "reconstruct-lock-"));
	const dataDir = join(root, "data");
	return { root, dataDir, lockDir: `${dataDir}.lock` };
}

function writeLockOwner(lockDir: string, record: Record<string, unknown>): void {
	mkdirSync(lockDir, { recursive: true });
	writeFileSync(join(lockDir, LOCK_OWNER_FILE), `${JSON.stringify(record)}\n`, "utf8");
}

function readLockOwner(lockDir: string): Record<string, unknown> {
	return asRecord(JSON.parse(readFileSync(join(lockDir, LOCK_OWNER_FILE), "utf8")));
}

function lockOwnerRecord(overrides: Record<string, unknown> = {}): Record<string, unknown> {
	return {
		version: 1,
		pid: process.pid,
		token: crypto.randomUUID(),
		createdAtMs: Date.now(),
		phase: "held",
		...overrides,
	};
}

function lockArtifacts(parentDir: string): string[] {
	return readdirSync(parentDir)
		.filter((name) => name.startsWith("data.lock"))
		.sort();
}

function deadPid(): number {
	for (let attempt = 0; attempt < 5; attempt += 1) {
		const child = spawnSync(process.execPath, ["--version"], { stdio: "ignore" });
		const pid = child.pid;
		if (typeof pid !== "number") continue;
		try {
			process.kill(pid, 0);
		} catch (error) {
			if (error instanceof Error && "code" in error && error.code === "ESRCH") return pid;
		}
	}
	throw new Error("failed to obtain a provably dead pid for lock fixtures");
}

async function captureError(promise: Promise<unknown>): Promise<Error> {
	try {
		await promise;
	} catch (error) {
		if (error instanceof Error) return error;
		throw new Error(`expected an Error rejection, got: ${String(error)}`);
	}
	throw new Error("expected promise to reject");
}

// These lock tests exercise the real OS-backed mkdir/rename/stat contention
// protocol and its wall-clock acquisition bound; fake timers cannot advance
// the competing filesystem operations or the production retry sleeps.
describe("reconstruction data-directory lock stale recovery (N16/N21)", () => {
	test("waiter on a live holder times out at its configured bound with owner diagnostics", async () => {
		const fx = makeLockFixture();
		try {
			const holder = await acquireDataDirectoryLock(fx.dataDir, 5_000);
			expect(holder.lockDir).toBe(fx.lockDir);
			expect(holder.token.length).toBeGreaterThan(0);
			expect(readLockOwner(fx.lockDir).phase).toBe("held");

			const startedAt = Date.now();
			const error = await captureError(acquireDataDirectoryLock(fx.dataDir, 200));
			const waitedMs = Date.now() - startedAt;
			expect(error.message).toContain("timed out acquiring reconstruction lock");
			expect(error.message).toContain("(bound 200ms)");
			expect(error.message).toContain(`live owner pid ${process.pid}`);
			expect(waitedMs).toBeGreaterThanOrEqual(180);
			expect(waitedMs).toBeLessThan(5_000);
			// The live holder was never reaped while the waiter spun.
			expect(readLockOwner(fx.lockDir).token).toBe(holder.token);

			await releaseDataDirectoryLock(holder);
			expect(lockArtifacts(fx.root)).toEqual([]);
		} finally {
			rmSync(fx.root, { recursive: true, force: true });
		}
	});

	test("a live owner is never reaped merely because its lock is old", async () => {
		const fx = makeLockFixture();
		try {
			const ancientMs = Date.now() - 3_600_000;
			const liveToken = crypto.randomUUID();
			writeLockOwner(fx.lockDir, lockOwnerRecord({ token: liveToken, createdAtMs: ancientMs }));
			const past = new Date(ancientMs);
			utimesSync(fx.lockDir, past, past);

			const error = await captureError(acquireDataDirectoryLock(fx.dataDir, 200));
			expect(error.message).toContain(`live owner pid ${process.pid}`);
			// Old but alive: the canonical lock is untouched and never quarantined.
			expect(lockArtifacts(fx.root)).toEqual(["data.lock"]);
			expect(readLockOwner(fx.lockDir).token).toBe(liveToken);
		} finally {
			rmSync(fx.root, { recursive: true, force: true });
		}
	});

	test("reconstructProviderData bounds its lock wait via lockAcquireTimeoutMs and validates it", async () => {
		const catalog: ProviderCatalog = { alpha: { model: { id: "model" } } };
		const fixture = makeFixture(catalog);
		try {
			const holder = await acquireDataDirectoryLock(fixture.dataDir, 5_000);
			try {
				await expect(
					reconstructProviderData({
						repoRoot: fixture.root,
						catalogPath: fixture.catalogPath,
						providersDir: fixture.providersDir,
						dataDir: fixture.dataDir,
						inversionProof: noopProof,
						lockAcquireTimeoutMs: 150,
					}),
				).rejects.toThrow("timed out acquiring reconstruction lock");
			} finally {
				await releaseDataDirectoryLock(holder);
			}

			for (const bad of [0, -1, 2.5, Number.NaN, Number.POSITIVE_INFINITY]) {
				await expect(
					reconstructProviderData({
						repoRoot: fixture.root,
						catalogPath: fixture.catalogPath,
						providersDir: fixture.providersDir,
						dataDir: fixture.dataDir,
						inversionProof: noopProof,
						lockAcquireTimeoutMs: bad,
					}),
				).rejects.toThrow("lockAcquireTimeoutMs must be a finite positive integer");
			}
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("a valid dead-owner lock is recovered and reconstruction completes", async () => {
		const catalog: ProviderCatalog = { alpha: { model: { id: "model", v: 1 } } };
		const fixture = makeFixture(catalog);
		try {
			writeLockOwner(
				`${fixture.dataDir}.lock`,
				lockOwnerRecord({ pid: deadPid(), createdAtMs: Date.now() - 60_000 }),
			);

			const result = await reconstructProviderData({
				repoRoot: fixture.root,
				catalogPath: fixture.catalogPath,
				providersDir: fixture.providersDir,
				dataDir: fixture.dataDir,
				inversionProof: noopProof,
				lockAcquireTimeoutMs: 3_000,
			});

			expect(result.written).toBe(1);
			expect(readdirSync(fixture.dataDir)).toEqual(["alpha.json"]);
			expect(lockArtifacts(fixture.providersDir)).toEqual([]);
			expect(siblingArtifacts(fixture.providersDir)).toEqual([]);
		} finally {
			rmSync(fixture.root, { recursive: true, force: true });
		}
	});

	test("two simultaneous stale reapers yield exactly one held critical section", async () => {
		const fx = makeLockFixture();
		try {
			writeLockOwner(
				fx.lockDir,
				lockOwnerRecord({ pid: deadPid(), createdAtMs: Date.now() - 60_000 }),
			);
			let held = 0;
			let maxConcurrent = 0;
			let sections = 0;
			const contend = async (): Promise<void> => {
				const handle = await acquireDataDirectoryLock(fx.dataDir, 5_000);
				held += 1;
				sections += 1;
				maxConcurrent = Math.max(maxConcurrent, held);
				// Real hold window: gives the rival reaper wall-clock time to
				// (incorrectly) acquire concurrently; fake timers cannot advance
				// its OS-backed polling loop.
				await Bun.sleep(25);
				held -= 1;
				await releaseDataDirectoryLock(handle);
			};

			await Promise.all([contend(), contend()]);

			expect(maxConcurrent).toBe(1);
			expect(sections).toBe(2);
			expect(lockArtifacts(fx.root)).toEqual([]);
		} finally {
			rmSync(fx.root, { recursive: true, force: true });
		}
	});

	test("injected A→B ABA swap never deletes the live replacement and blocks claimants until release", async () => {
		const fx = makeLockFixture();
		try {
			const staleTokenA = crypto.randomUUID();
			const liveTokenB = crypto.randomUUID();
			// The reaper observed stale token A; before its rename that owner
			// vanished and live owner B acquired the canonical path.
			writeLockOwner(fx.lockDir, lockOwnerRecord({ token: liveTokenB }));

			const recovered = await recoverStaleLock(fx.dataDir, staleTokenA);
			expect(recovered).toBe(false);

			const artifacts = lockArtifacts(fx.root);
			expect(artifacts).toHaveLength(1);
			const quarantineName = artifacts[0] ?? "";
			expect(quarantineName.startsWith("data.lock.reap-")).toBe(true);
			const quarantineDir = join(fx.root, quarantineName);
			expect(readLockOwner(quarantineDir).token).toBe(liveTokenB);

			// No third claimant becomes held while B sits in quarantine.
			const error = await captureError(acquireDataDirectoryLock(fx.dataDir, 250));
			expect(error.message).toContain("timed out acquiring reconstruction lock");
			expect(readLockOwner(quarantineDir).token).toBe(liveTokenB);
			// The failed claimant withdrew its provisional canonical directory.
			expect(lockArtifacts(fx.root)).toEqual([quarantineName]);

			// B's token-verified release removes its quarantined directory...
			await releaseDataDirectoryLock({ lockDir: fx.lockDir, token: liveTokenB });
			expect(lockArtifacts(fx.root)).toEqual([]);

			// ...which lifts the barrier for the next claimant.
			const successor = await acquireDataDirectoryLock(fx.dataDir, 1_000);
			await releaseDataDirectoryLock(successor);
			expect(lockArtifacts(fx.root)).toEqual([]);
		} finally {
			rmSync(fx.root, { recursive: true, force: true });
		}
	});

	test("a reaped owner's delayed release cannot delete the successor's lock", async () => {
		const fx = makeLockFixture();
		try {
			const old = await acquireDataDirectoryLock(fx.dataDir, 1_000);
			// The old owner "dies" without releasing: same token, dead pid.
			writeLockOwner(
				fx.lockDir,
				lockOwnerRecord({ token: old.token, pid: deadPid(), createdAtMs: Date.now() - 60_000 }),
			);

			const successor = await acquireDataDirectoryLock(fx.dataDir, 3_000);
			expect(successor.token).not.toBe(old.token);

			// Delayed finally from the reaped owner: tokens differ, nothing removed.
			await releaseDataDirectoryLock(old);
			expect(readLockOwner(fx.lockDir).token).toBe(successor.token);
			expect(readLockOwner(fx.lockDir).phase).toBe("held");

			await releaseDataDirectoryLock(successor);
			expect(lockArtifacts(fx.root)).toEqual([]);
		} finally {
			rmSync(fx.root, { recursive: true, force: true });
		}
	}, 20_000);

	test("invalid metadata inside the initializing grace is waited on, never reaped", async () => {
		const fx = makeLockFixture();
		try {
			mkdirSync(fx.lockDir);
			writeFileSync(join(fx.lockDir, LOCK_OWNER_FILE), "{not json", "utf8");

			const error = await captureError(acquireDataDirectoryLock(fx.dataDir, 250));
			expect(error.message).toContain("timed out acquiring reconstruction lock");
			expect(error.message).toContain("treated as initializing");
			expect(readFileSync(join(fx.lockDir, LOCK_OWNER_FILE), "utf8")).toBe("{not json");
			expect(lockArtifacts(fx.root)).toEqual(["data.lock"]);
		} finally {
			rmSync(fx.root, { recursive: true, force: true });
		}
	});

	test("an abandoned lock with unwritten metadata is recovered after the grace", async () => {
		const fx = makeLockFixture();
		try {
			mkdirSync(fx.lockDir); // owner metadata never written
			const past = new Date(Date.now() - 60_000);
			utimesSync(fx.lockDir, past, past);

			const handle = await acquireDataDirectoryLock(fx.dataDir, 2_000);
			expect(readLockOwner(fx.lockDir).token).toBe(handle.token);
			expect(readLockOwner(fx.lockDir).phase).toBe("held");
			await releaseDataDirectoryLock(handle);
			expect(lockArtifacts(fx.root)).toEqual([]);
		} finally {
			rmSync(fx.root, { recursive: true, force: true });
		}
	});

	test("an orphaned dead quarantine is swept so acquisition proceeds", async () => {
		const fx = makeLockFixture();
		try {
			writeLockOwner(
				`${fx.lockDir}.reap-${crypto.randomUUID()}`,
				lockOwnerRecord({ pid: deadPid(), createdAtMs: Date.now() - 60_000 }),
			);

			const handle = await acquireDataDirectoryLock(fx.dataDir, 2_000);
			await releaseDataDirectoryLock(handle);
			expect(lockArtifacts(fx.root)).toEqual([]);
		} finally {
			rmSync(fx.root, { recursive: true, force: true });
		}
	});
});
