/**
 * Reconstruct the reference tree's gitignored provider data JSONs
 * (`.references/pi/packages/ai/src/providers/data/*.json`) from the committed
 * Rust catalog (`crates/pi-ai/data/builtin-models.json`).
 *
 * Upstream produces these files with a network-fetching generator; this repo
 * forbids network generation, and the committed catalog is the proven-exact
 * offline inverse (regenerating `builtin-models.json` from the reconstructed
 * files yields a zero diff). Harnesses that build the reference TypeScript pi
 * run this first so `generate-models` never has to run.
 *
 * Publication is transactional: candidates are written to a unique sibling
 * staging directory, validated, then swapped into place via rename while the
 * previous live tree (if any) is retained as a unique sibling backup until the
 * inversion proof succeeds.
 */
import { access, mkdir, readdir, rename, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const DEFAULT_CATALOG_PATH = join(REPO_ROOT, "crates/pi-ai/data/builtin-models.json");
const DEFAULT_PROVIDERS_DIR = join(
	REPO_ROOT,
	".references/pi/packages/ai/src/providers",
);
const DEFAULT_DATA_DIR = join(DEFAULT_PROVIDERS_DIR, "data");
const DATA_DIRECTORY_LOCK_RETRY_MS = 10;
type FileSystemError = Error & { code?: string };

export type ProviderCatalog = Record<string, Record<string, unknown>>;

export type ReconstructProviderDataOptions = {
	repoRoot?: string;
	catalogPath?: string;
	providersDir?: string;
	dataDir?: string;
	/**
	 * Inversion authority hook. When omitted, the default proof (spawning
	 * `scripts/generate-builtin-models.ts` and comparing `catalogPath`
	 * byte-for-byte) is used only when paths are the repository defaults —
	 * that generator only rewrites the hard-coded default catalog, so custom
	 * `catalogPath`/`providersDir`/`dataDir`/`repoRoot` would otherwise pass
	 * vacuously. Custom paths require an explicit proof. Tests may inject a
	 * failing or no-op proof without touching the checkout generator.
	 */
	inversionProof?: (ctx: ReconstructProofContext) => Promise<void>;
	/**
	 * Post-proof backup cleanup hook. Defaults to recursive force-removal of the
	 * backup sibling. inversionProof success is the commit point: a cleanup
	 * failure must surface without rolling the published live tree back to the
	 * stale backup. Tests may inject a failing cleanup through this public seam.
	 */
	removeBackup?: (backupDir: string) => Promise<void>;
};

export type ReconstructProofContext = {
	repoRoot: string;
	catalogPath: string;
	providersDir: string;
	dataDir: string;
};

export type ReconstructProviderDataResult = {
	written: number;
	providers: string[];
	dataDir: string;
};

function sortDeep(value: unknown): unknown {
	if (Array.isArray(value)) return value.map(sortDeep);
	if (value === null || typeof value !== "object") return value;
	const sorted = Object.create(null) as Record<string, unknown>;
	for (const key of Object.keys(value as Record<string, unknown>).sort()) {
		sorted[key] = sortDeep((value as Record<string, unknown>)[key]);
	}
	return sorted;
}

function encodeProviderModels(models: Record<string, unknown>): string {
	return `${JSON.stringify(sortDeep(models), null, "\t")}\n`;
}

function setKey(values: Iterable<string>): string {
	return [...values].sort().join("\0");
}

async function pathExists(path: string): Promise<boolean> {
	try {
		await access(path);
		return true;
	} catch {
		return false;
	}
}

function uniqueSibling(dataDir: string, kind: "staging" | "backup"): string {
	return `${dataDir}.${kind}.${process.pid}.${Date.now()}.${Math.random().toString(16).slice(2, 10)}`;
}


async function acquireDataDirectoryLock(dataDir: string): Promise<string> {
	const lockDir = `${dataDir}.lock`;
	for (;;) {
		try {
			await mkdir(lockDir);
			return lockDir;
		} catch (error) {
			const errorCode = error instanceof Error ? (error as FileSystemError).code : undefined;
			if (errorCode !== "EEXIST") {
				const detail = error instanceof Error ? error.message : String(error);
				throw new Error(`failed to acquire reconstruction lock ${lockDir}: ${detail}`);
			}
			await Bun.sleep(DATA_DIRECTORY_LOCK_RETRY_MS);
		}
	}
}

async function releaseDataDirectoryLock(lockDir: string): Promise<void> {
	try {
		await rm(lockDir, { recursive: true, force: false });
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		throw new Error(`failed to release reconstruction lock ${lockDir}: ${detail}`);
	}
}

async function removeIfExists(path: string | null | undefined): Promise<void> {
	if (!path) return;
	await rm(path, { recursive: true, force: true });
}

async function listWrapperProviders(providersDir: string): Promise<string[]> {
	const names = await readdir(providersDir);
	return names
		.filter((name) => name.endsWith(".models.ts"))
		.map((name) => name.replace(/\.models\.ts$/, ""))
		.sort();
}

function assertBidirectionalProviderSets(
	wrappers: string[],
	catalogProviders: string[],
): void {
	const wrapperKey = setKey(wrappers);
	const catalogKey = setKey(catalogProviders);
	if (wrapperKey === catalogKey) return;

	const wrapperSet = new Set(wrappers);
	const catalogSet = new Set(catalogProviders);
	const missingFromCatalog = wrappers.filter((id) => !catalogSet.has(id));
	const missingFromWrappers = catalogProviders.filter((id) => !wrapperSet.has(id));
	const parts: string[] = [];
	if (missingFromCatalog.length > 0) {
		parts.push(`wrappers missing from catalog: ${missingFromCatalog.join(", ")}`);
	}
	if (missingFromWrappers.length > 0) {
		parts.push(`catalog providers missing wrappers: ${missingFromWrappers.join(", ")}`);
	}
	throw new Error(
		`wrapper/catalog provider set mismatch (${parts.join("; ") || "unordered divergence"})`,
	);
}

async function validateStagingDirectory(
	stagingDir: string,
	expectedProviders: string[],
	catalog: ProviderCatalog,
): Promise<void> {
	const stagedNames = await readdir(stagingDir);
	const expectedNames = expectedProviders.map((id) => `${id}.json`).sort();
	const actualNames = [...stagedNames].sort();
	if (setKey(actualNames) !== setKey(expectedNames)) {
		throw new Error(
			`staging directory file set mismatch: expected [${expectedNames.join(", ")}] but found [${actualNames.join(", ")}]`,
		);
	}

	for (const provider of expectedProviders) {
		const fileName = `${provider}.json`;
		const filePath = join(stagingDir, fileName);
		const raw = await Bun.file(filePath).text();
		let parsed: unknown;
		try {
			parsed = JSON.parse(raw);
		} catch (error) {
			const detail = error instanceof Error ? error.message : String(error);
			throw new Error(`staged ${fileName} does not parse as JSON: ${detail}`);
		}
		if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
			throw new Error(`staged ${fileName} root must be a JSON object`);
		}
		const models = catalog[provider];
		if (models === undefined) {
			throw new Error(`catalog lost provider "${provider}" during staging validation`);
		}
		const expected = encodeProviderModels(models);
		if (raw !== expected) {
			throw new Error(
				`staged ${fileName} is not exact sorted catalog content for provider "${provider}"`,
			);
		}
	}
}

function usesRepositoryDefaultPaths(ctx: ReconstructProofContext): boolean {
	// generate-builtin-models.ts only reads/writes the hard-coded default
	// checkout paths, so the default inversion proof is meaningful only when
	// reconstruction targets those same paths.
	return (
		resolve(ctx.repoRoot) === resolve(REPO_ROOT) &&
		resolve(ctx.catalogPath) === resolve(DEFAULT_CATALOG_PATH) &&
		resolve(ctx.providersDir) === resolve(DEFAULT_PROVIDERS_DIR) &&
		resolve(ctx.dataDir) === resolve(DEFAULT_DATA_DIR)
	);
}

async function defaultInversionProof(ctx: ReconstructProofContext): Promise<void> {
	// Fail-fast inversion proof: regenerating the catalog from the
	// reconstructed files must reproduce the exact catalog text. Normalize
	// CRLF only because Windows checkout conversion is not generator drift.
	// Snapshot/restore keeps the generator-owned artifact untouched.
	// Only valid for repository default paths — see usesRepositoryDefaultPaths.
	const before = await Bun.file(ctx.catalogPath).bytes();
	try {
		const regen = Bun.spawnSync(
			[process.execPath, "run", join(ctx.repoRoot, "scripts/generate-builtin-models.ts")],
			{ cwd: ctx.repoRoot, stdout: "ignore", stderr: "pipe" },
		);
		if (regen.exitCode !== 0) {
			throw new Error(
				`inversion proof failed: generate-builtin-models exited ${regen.exitCode}: ${regen.stderr.toString().slice(0, 400)}`,
			);
		}
		const after = await Bun.file(ctx.catalogPath).bytes();
		const normalizeCrLf = (bytes: Uint8Array): string =>
			Buffer.from(bytes).toString("utf8").replaceAll("\r\n", "\n");
		if (normalizeCrLf(before) !== normalizeCrLf(after)) {
			throw new Error(
				"inversion proof failed: regenerated builtin-models.json differs from the catalog the reconstruction used",
			);
		}
	} finally {
		await writeFile(ctx.catalogPath, before);
	}
}

/**
 * Reconstruct provider data JSONs transactionally.
 *
 * Never writes directly into the live data directory: candidates are staged,
 * validated, published by rename, then inversion-proved while a sibling lock
 * serializes every observation and mutation of that data directory. Successful
 * `inversionProof` is the commit point. On proof or rename failure the previous
 * live tree is restored byte-for-byte (or removed when it was initially absent)
 * and staging/backup siblings are cleaned up. After commit, backup cleanup
 * failures are surfaced without restoring the stale pre-publish tree.
 */
export async function reconstructProviderData(
	options: ReconstructProviderDataOptions = {},
): Promise<ReconstructProviderDataResult> {
	const repoRoot = options.repoRoot ?? REPO_ROOT;
	const catalogPath = options.catalogPath ?? DEFAULT_CATALOG_PATH;
	const providersDir = options.providersDir ?? DEFAULT_PROVIDERS_DIR;
	const dataDir = options.dataDir ?? DEFAULT_DATA_DIR;
	const proofCtx: ReconstructProofContext = {
		repoRoot,
		catalogPath,
		providersDir,
		dataDir,
	};
	let inversionProof = options.inversionProof;
	if (inversionProof === undefined) {
		if (!usesRepositoryDefaultPaths(proofCtx)) {
			throw new Error(
				"default inversion proof only covers repository default paths (catalog/providers/data under this checkout); pass an explicit inversionProof when reconstructing with custom paths",
			);
		}
		inversionProof = defaultInversionProof;
	}
	const removeBackup = options.removeBackup ?? removeIfExists;

	const catalog = (await Bun.file(catalogPath).json()) as ProviderCatalog;
	if (catalog === null || typeof catalog !== "object" || Array.isArray(catalog)) {
		throw new Error(`catalog root must be a JSON object: ${catalogPath}`);
	}

	const wrappers = await listWrapperProviders(providersDir);
	const catalogProviders = Object.keys(catalog).sort();
	assertBidirectionalProviderSets(wrappers, catalogProviders);

	const expectedBodies = new Map<string, string>();
	for (const provider of wrappers) {
		const models = catalog[provider];
		if (models === undefined) {
			throw new Error(`catalog has no models for wrapper provider: ${provider}`);
		}
		if (models === null || typeof models !== "object" || Array.isArray(models)) {
			throw new Error(`catalog provider "${provider}" must be a model object map`);
		}
		expectedBodies.set(provider, encodeProviderModels(models));
	}

	const lockDir = await acquireDataDirectoryLock(dataDir);
	try {
		const hadLive = await pathExists(dataDir);
		const stagingDir = uniqueSibling(dataDir, "staging");
		let backupDir: string | null = null;
		let published = false;
		let committed = false;
		let stagingPending = true;

		try {
			await mkdir(stagingDir, { recursive: true });
			for (const provider of wrappers) {
				const body = expectedBodies.get(provider);
				if (body === undefined) {
					throw new Error(`missing encoded body for provider "${provider}"`);
				}
				await writeFile(join(stagingDir, `${provider}.json`), body, "utf8");
			}

			await validateStagingDirectory(stagingDir, wrappers, catalog);

			if (hadLive) {
				backupDir = uniqueSibling(dataDir, "backup");
				try {
					await rename(dataDir, backupDir);
				} catch (error) {
					const detail = error instanceof Error ? error.message : String(error);
					throw new Error(`failed to rename live data to backup: ${detail}`);
				}
			}

			try {
				await rename(stagingDir, dataDir);
				stagingPending = false;
				published = true;
			} catch (error) {
				const detail = error instanceof Error ? error.message : String(error);
				throw new Error(`failed to publish staging directory to live data: ${detail}`);
			}

			await inversionProof(proofCtx);
			// Successful inversion proof commits the published live tree.
			committed = true;
		} catch (error) {
			// Pre-commit only: restore the pre-publish live tree byte-for-byte (or absent).
			if (!committed) {
				try {
					if (published) {
						await removeIfExists(dataDir);
						if (backupDir !== null) {
							await rename(backupDir, dataDir);
							backupDir = null;
						}
					} else if (backupDir !== null && !(await pathExists(dataDir))) {
						await rename(backupDir, dataDir);
						backupDir = null;
					}
				} catch (restoreError) {
					const primary = error instanceof Error ? error.message : String(error);
					const secondary =
						restoreError instanceof Error ? restoreError.message : String(restoreError);
					throw new Error(
						`reconstruction failed (${primary}); additionally failed to restore live data: ${secondary}`,
					);
				}

				if (stagingPending) {
					await removeIfExists(stagingDir);
				}
				await removeIfExists(backupDir);
			}
			throw error;
		}

		if (backupDir !== null) {
			const leftoverBackup = backupDir;
			try {
				await removeBackup(leftoverBackup);
				backupDir = null;
			} catch (cleanupError) {
				const detail =
					cleanupError instanceof Error ? cleanupError.message : String(cleanupError);
				throw new Error(
					`reconstruction published successfully but failed to remove backup ${leftoverBackup}: ${detail}`,
				);
			}
		}

		console.warn(`reconstructed ${wrappers.length} provider data files from the catalog`);
		if (options.inversionProof === undefined) {
			console.warn("inversion proof passed: catalog round-trips exactly");
		}

		return {
			written: wrappers.length,
			providers: wrappers,
			dataDir,
		};
	} finally {
		await releaseDataDirectoryLock(lockDir);
	}
}

async function main(): Promise<void> {
	await reconstructProviderData();
}

if (import.meta.main) {
	await main();
}
