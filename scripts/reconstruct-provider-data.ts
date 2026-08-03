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
import { createHash } from "node:crypto";
import {
	access,
	mkdir,
	readdir,
	readFile,
	rename,
	rm,
	stat,
	writeFile,
} from "node:fs/promises";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const DEFAULT_CATALOG_PATH = join(REPO_ROOT, "crates/pi-ai/data/builtin-models.json");
const DEFAULT_PROVIDERS_DIR = join(
	REPO_ROOT,
	".references/pi/packages/ai/src/providers",
);
const DEFAULT_DATA_DIR = join(DEFAULT_PROVIDERS_DIR, "data");
const DATA_DIRECTORY_LOCK_RETRY_MS = 10;
const DEFAULT_LOCK_ACQUIRE_TIMEOUT_MS = 10_000;
const LOCK_INITIALIZING_GRACE_MS = 30_000;
const LOCK_OWNER_FILE = "owner.json";
const DATA_MANIFEST_FILE = ".manifest.json";
const LOCK_OWNER_VERSION = 1;
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
	/**
	 * Live-tree backup hook. Defaults to the atomic sibling rename; tests may
	 * inject a failed backup without moving the live tree.
	 */
	backupLive?: (dataDir: string, backupDir: string) => Promise<void>;
	/**
	 * Staging publication hook. Defaults to the atomic sibling rename; tests may
	 * inject a failed publish that leaves an unexpected live path in place.
	 */
	publishStaging?: (stagingDir: string, dataDir: string) => Promise<void>;
	/**
	 * Lock release hook. Defaults to token-verified lock removal; tests may
	 * inject cleanup failure without altering the lock protocol itself.
	 */
	releaseLock?: (handle: DataDirectoryLockHandle) => Promise<void>;
	/**
	 * Bounded lock acquisition: total milliseconds to wait for the data
	 * directory lock before failing with owner/bounded-wait diagnostics. Must
	 * be a finite positive integer; defaults to 10_000. A live lock owner is
	 * never reaped — waiters simply time out at this bound.
	 */
	lockAcquireTimeoutMs?: number;
	/**
	 * Optional test-observability hook invoked synchronously inside
	 * `acquireDataDirectoryLock` only when an active-owner inspection returns
	 * `kind === "wait"`, immediately before the retry sleep. Production callers
	 * omit it; tests use a deferred barrier to prove a second contender observed
	 * a held lock before the first owner releases it, without wall-clock sleep.
	 */
	onLockWait?: () => void;
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

/**
 * Ownership handle for the reconstruction data-directory lock. `token` is the
 * acquisition's private random identity; release removes only directories
 * whose stored owner metadata still carries this exact token.
 */
export type DataDirectoryLockHandle = {
	lockDir: string;
	token: string;
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

function groupProviderModels(
	provider: string,
	models: Record<string, unknown>,
): Record<string, Record<string, unknown>> {
	const groups = Object.create(null) as Record<string, Record<string, unknown>>;
	for (const [modelId, value] of Object.entries(models)) {
		if (value === null || typeof value !== "object" || Array.isArray(value)) {
			throw new Error(`catalog model "${provider}/${modelId}" must be an object`);
		}
		const model = value as Record<string, unknown>;
		const api = model["api"];
		if (typeof api !== "string" || api.length === 0) {
			throw new Error(`catalog model "${provider}/${modelId}" must have a non-empty api`);
		}
		const group = groups[api] ?? (groups[api] = Object.create(null) as Record<string, unknown>);
		group[modelId] = model;
	}
	return groups;
}

function encodeProviderModels(provider: string, models: Record<string, unknown>): string {
	return `${JSON.stringify(sortDeep(groupProviderModels(provider, models)), null, "\t")}\n`;
}

function rebuildProviderManifest(
	catalog: ProviderCatalog,
	bodies: ReadonlyMap<string, string>,
	previous: Uint8Array,
): Uint8Array {
	let parsed: unknown;
	try {
		parsed = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(previous));
	} catch (error) {
		throw new Error(
			`provider manifest is not valid UTF-8 JSON: ${error instanceof Error ? error.message : String(error)}`,
		);
	}
	if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
		throw new Error("provider manifest must contain a JSON object");
	}
	const generatedAt = (parsed as Record<string, unknown>)["generatedAt"];
	if (typeof generatedAt !== "string" || Number.isNaN(Date.parse(generatedAt))) {
		throw new Error("provider manifest has an invalid generation timestamp");
	}

	const structure = Object.create(null) as Record<string, Record<string, string>>;
	for (const provider of Object.keys(catalog).sort()) {
		const models = catalog[provider];
		if (models === undefined) throw new Error(`catalog lost provider "${provider}"`);
		const providerStructure = Object.create(null) as Record<string, string>;
		for (const [api, group] of Object.entries(groupProviderModels(provider, models))) {
			for (const modelId of Object.keys(group)) providerStructure[modelId] = api;
		}
		structure[provider] = providerStructure;
	}

	const files = Object.create(null) as Record<string, string>;
	for (const [provider, body] of [...bodies.entries()].sort(([left], [right]) =>
		left < right ? -1 : left > right ? 1 : 0
	)) {
		files[`${provider}.json`] = createHash("sha256").update(body).digest("hex");
	}
	const structureHash = createHash("sha256")
		.update(JSON.stringify(sortDeep(structure)))
		.digest("hex");
	const manifest = {
		schemaVersion: 3,
		generatedAt,
		structureHash,
		files,
	};
	return new TextEncoder().encode(`${JSON.stringify(manifest)}\n`);
}

function setKey(values: Iterable<string>): string {
	return [...values].sort().join("\0");
}

function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
	if (left.byteLength !== right.byteLength) return false;
	for (let index = 0; index < left.byteLength; index += 1) {
		if (left[index] !== right[index]) return false;
	}
	return true;
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


type LockOwnerRecord = {
	version: typeof LOCK_OWNER_VERSION;
	pid: number;
	token: string;
	createdAtMs: number;
	phase: "initializing" | "held";
};

type LockContention =
	| { kind: "retry" }
	| { kind: "wait"; detail: string }
	| { kind: "stale"; observedToken: string | null; detail: string };

function errorCode(error: unknown): string | undefined {
	return error instanceof Error ? (error as FileSystemError).code : undefined;
}

function errorDetail(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

/** ESRCH alone proves death; success or EPERM (or anything else) counts as live. */
function isProcessAlive(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch (error) {
		return errorCode(error) !== "ESRCH";
	}
}

/**
 * Strictly parse a versioned lock owner record. Anything short of the exact
 * shape is invalid and is treated as still-initializing metadata; parsed
 * values are identity only and never supply filesystem paths.
 */
function parseLockOwnerRecord(raw: string): LockOwnerRecord | null {
	let value: unknown;
	try {
		value = JSON.parse(raw);
	} catch {
		return null;
	}
	if (value === null || typeof value !== "object" || Array.isArray(value)) return null;
	const candidate = value as Record<string, unknown>;
	if (Object.keys(candidate).sort().join(",") !== "createdAtMs,phase,pid,token,version") {
		return null;
	}
	const { version, pid, token, createdAtMs, phase } = candidate;
	if (version !== LOCK_OWNER_VERSION) return null;
	if (typeof pid !== "number" || !Number.isSafeInteger(pid) || pid <= 0) return null;
	if (typeof token !== "string" || token.length === 0) return null;
	if (typeof createdAtMs !== "number" || !Number.isFinite(createdAtMs) || createdAtMs < 0) {
		return null;
	}
	if (phase !== "initializing" && phase !== "held") return null;
	return { version: LOCK_OWNER_VERSION, pid, token, createdAtMs, phase };
}

async function readLockOwnerRecord(dir: string): Promise<LockOwnerRecord | null> {
	let raw: string;
	try {
		raw = await readFile(join(dir, LOCK_OWNER_FILE), "utf8");
	} catch {
		return null;
	}
	return parseLockOwnerRecord(raw);
}

/** Atomic within the lock directory: temp write, then same-directory rename. */
async function writeLockOwnerRecord(dir: string, record: LockOwnerRecord): Promise<void> {
	const target = join(dir, LOCK_OWNER_FILE);
	const temp = `${target}.${record.token}.tmp`;
	await writeFile(temp, `${JSON.stringify(record)}\n`, "utf8");
	await rename(temp, target);
}

/** Quarantine sibling paths, derived from the canonical lock path by listing. */
async function listQuarantineDirs(lockDir: string): Promise<string[]> {
	const parent = dirname(lockDir);
	const prefix = `${basename(lockDir)}.reap-`;
	let names: string[];
	try {
		names = await readdir(parent);
	} catch {
		return [];
	}
	return names
		.filter((name) => name.startsWith(prefix))
		.sort()
		.map((name) => join(parent, name));
}

/**
 * Remove quarantined lock directories whose owner is provably gone: a valid
 * record with a dead pid, or invalid metadata older than the initializing
 * grace. A quarantine with a live owner is left untouched — it is the barrier
 * that keeps claimants provisional until that owner releases it.
 */
async function sweepDeadQuarantines(lockDir: string): Promise<void> {
	for (const quarantineDir of await listQuarantineDirs(lockDir)) {
		const record = await readLockOwnerRecord(quarantineDir);
		if (record === null) {
			let mtimeMs: number;
			try {
				mtimeMs = (await stat(quarantineDir)).mtimeMs;
			} catch {
				continue;
			}
			if (Date.now() - mtimeMs < LOCK_INITIALIZING_GRACE_MS) continue;
		} else if (isProcessAlive(record.pid)) {
			continue;
		}
		await rm(quarantineDir, { recursive: true, force: true });
	}
}

/**
 * Remove only directories whose stored owner token is exactly `token`, at the
 * canonical path or in any quarantine. A missing directory or a different
 * token means ownership was lost; that is never an instruction to remove
 * another owner's directory.
 */
async function removeOwnedLockDirectories(lockDir: string, token: string): Promise<void> {
	for (const dir of [lockDir, ...(await listQuarantineDirs(lockDir))]) {
		const record = await readLockOwnerRecord(dir);
		if (record === null || record.token !== token) continue;
		await rm(dir, { recursive: true, force: true });
	}
}

async function confirmProvisionalOwnership(lockDir: string, token: string): Promise<boolean> {
	if ((await listQuarantineDirs(lockDir)).length > 0) return false;
	const canonical = await readLockOwnerRecord(lockDir);
	return canonical !== null && canonical.token === token;
}

/**
 * Complete a provisional claim on a freshly created lock directory. The
 * acquirer stays `initializing` until it proves that no reaper quarantine
 * exists and that the canonical metadata still carries its exact token; only
 * then does it promote to `held`, re-verify, and return ownership. Any failed
 * check withdraws the exact-token directories and reports the claim as lost,
 * closing the observe→rename ABA window from the claimant's side.
 */
async function claimLockDirectory(lockDir: string, token: string): Promise<boolean> {
	const record: LockOwnerRecord = {
		version: LOCK_OWNER_VERSION,
		pid: process.pid,
		token,
		createdAtMs: Date.now(),
		phase: "initializing",
	};
	try {
		await writeLockOwnerRecord(lockDir, record);
		if (!(await confirmProvisionalOwnership(lockDir, token))) {
			await removeOwnedLockDirectories(lockDir, token);
			return false;
		}
		await writeLockOwnerRecord(lockDir, { ...record, phase: "held" });
		if (!(await confirmProvisionalOwnership(lockDir, token))) {
			await removeOwnedLockDirectories(lockDir, token);
			return false;
		}
		return true;
	} catch (error) {
		await removeOwnedLockDirectories(lockDir, token).catch(() => undefined);
		if (errorCode(error) === "ENOENT") return false;
		throw new Error(`failed to claim reconstruction lock ${lockDir}: ${errorDetail(error)}`);
	}
}

async function inspectLockOwner(lockDir: string): Promise<LockContention> {
	let mtimeMs: number;
	try {
		mtimeMs = (await stat(lockDir)).mtimeMs;
	} catch (error) {
		if (errorCode(error) === "ENOENT") return { kind: "retry" };
		throw new Error(`failed to inspect reconstruction lock ${lockDir}: ${errorDetail(error)}`);
	}
	const record = await readLockOwnerRecord(lockDir);
	if (record === null) {
		const ageMs = Math.max(0, Math.round(Date.now() - mtimeMs));
		if (ageMs < LOCK_INITIALIZING_GRACE_MS) {
			return {
				kind: "wait",
				detail: `owner metadata is missing or invalid (age ${ageMs}ms); treated as initializing within the ${LOCK_INITIALIZING_GRACE_MS}ms grace`,
			};
		}
		return {
			kind: "stale",
			observedToken: null,
			detail: `abandoned lock without valid owner metadata (age ${ageMs}ms)`,
		};
	}
	if (isProcessAlive(record.pid)) {
		return {
			kind: "wait",
			detail: `live owner pid ${record.pid} (phase ${record.phase}); a live owner is never reaped`,
		};
	}
	return {
		kind: "stale",
		observedToken: record.token,
		detail: `dead owner pid ${record.pid} (phase ${record.phase})`,
	};
}

/**
 * Race-safe takeover of a lock previously observed stale. The canonical
 * directory is atomically renamed to a unique same-directory quarantine and
 * its metadata re-read afterwards; it is deleted only when that post-rename
 * identity still matches the observed token (`null` = observed invalid).
 * Between observation and rename the old owner may have released and a live
 * replacement acquired the canonical path (ABA); a mismatched quarantine is
 * therefore never deleted here — its owner's token-verified release (or the
 * dead-owner sweep) removes it, and until then it blocks every provisional
 * claimant from becoming held.
 */
export async function recoverStaleLock(
	dataDir: string,
	observedToken: string | null,
): Promise<boolean> {
	const lockDir = `${dataDir}.lock`;
	const quarantineDir = `${lockDir}.reap-${crypto.randomUUID()}`;
	try {
		await rename(lockDir, quarantineDir);
	} catch (error) {
		if (errorCode(error) === "ENOENT") return false;
		throw new Error(
			`failed to quarantine stale reconstruction lock ${lockDir}: ${errorDetail(error)}`,
		);
	}
	const record = await readLockOwnerRecord(quarantineDir);
	const sameIdentity =
		observedToken === null ? record === null : record !== null && record.token === observedToken;
	if (!sameIdentity) return false;
	await rm(quarantineDir, { recursive: true, force: true });
	return true;
}

/**
 * Acquire the reconstruction lock for `dataDir` within a bounded wait. Owners
 * store versioned metadata; a live owner pid is never reaped no matter how
 * old, while dead-owner and abandoned-invalid locks are recovered through the
 * quarantine protocol in {@link recoverStaleLock}.
 */
export async function acquireDataDirectoryLock(
	dataDir: string,
	lockAcquireTimeoutMs: number,
	onLockWait?: () => void,
): Promise<DataDirectoryLockHandle> {
	const lockDir = `${dataDir}.lock`;
	const token = crypto.randomUUID();
	const startedAtMs = Date.now();
	let contention = "lock directory already exists";
	for (;;) {
		await sweepDeadQuarantines(lockDir);
		let created = false;
		try {
			await mkdir(lockDir);
			created = true;
		} catch (error) {
			if (errorCode(error) !== "EEXIST") {
				throw new Error(`failed to acquire reconstruction lock ${lockDir}: ${errorDetail(error)}`);
			}
		}
		let observedWait = false;
		if (created) {
			if (await claimLockDirectory(lockDir, token)) return { lockDir, token };
			contention =
				"withdrew provisional claim: an active quarantine or lost canonical ownership forced a retry";
		} else {
			const inspection = await inspectLockOwner(lockDir);
			if (inspection.kind === "stale") {
				contention = `recovering stale lock: ${inspection.detail}`;
				await recoverStaleLock(dataDir, inspection.observedToken);
			} else if (inspection.kind === "wait") {
				contention = inspection.detail;
				observedWait = true;
			}
		}
		const elapsedMs = Date.now() - startedAtMs;
		if (elapsedMs >= lockAcquireTimeoutMs) {
			throw new Error(
				`timed out acquiring reconstruction lock ${lockDir} after ${elapsedMs}ms (bound ${lockAcquireTimeoutMs}ms); ${contention}`,
			);
		}
		if (observedWait) onLockWait?.();
		await Bun.sleep(DATA_DIRECTORY_LOCK_RETRY_MS);
	}
}

/**
 * Release only directories that still carry this handle's exact token — the
 * canonical lock directory and any quarantine that captured it. ENOENT or a
 * different token means ownership was lost and removes nothing.
 */
export async function releaseDataDirectoryLock(handle: DataDirectoryLockHandle): Promise<void> {
	try {
		await removeOwnedLockDirectories(handle.lockDir, handle.token);
	} catch (error) {
		throw new Error(
			`failed to release reconstruction lock ${handle.lockDir}: ${errorDetail(error)}`,
		);
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
	expectedManifest: Uint8Array | null,
): Promise<void> {
	const stagedNames = await readdir(stagingDir);
	const expectedNames = expectedProviders.map((id) => `${id}.json`);
	if (expectedManifest !== null) expectedNames.push(DATA_MANIFEST_FILE);
	expectedNames.sort();
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
		const expected = encodeProviderModels(provider, models);
		if (raw !== expected) {
			throw new Error(
				`staged ${fileName} is not exact sorted catalog content for provider "${provider}"`,
			);
		}
	}

	if (expectedManifest !== null) {
		const actualManifest = await Bun.file(join(stagingDir, DATA_MANIFEST_FILE)).bytes();
		if (!bytesEqual(actualManifest, expectedManifest)) {
			throw new Error("staged provider manifest changed during reconstruction");
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

export async function defaultInversionProof(ctx: ReconstructProofContext): Promise<void> {
	// Fail-fast inversion proof: regenerating the catalog from the
	// reconstructed files must reproduce the exact catalog text. Normalize
	// CRLF only because Windows checkout conversion is not generator drift.
	// Snapshot/restore keeps the generator-owned artifact untouched.
	// Only valid for repository default paths — see usesRepositoryDefaultPaths.
	const before = await Bun.file(ctx.catalogPath).bytes();
	let proofFailure: unknown;
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
	} catch (error) {
		proofFailure = error;
		throw error;
	} finally {
		try {
			await writeFile(ctx.catalogPath, before);
		} catch (restoreError) {
			if (proofFailure === undefined) throw restoreError;
			throw new Error(
				`inversion proof failed (${errorDetail(proofFailure)}); additionally failed to restore catalog snapshot: ${errorDetail(restoreError)}`,
				{ cause: proofFailure },
			);
		}
	}
}

/**
 * Reconstruct provider data JSONs transactionally.
 *
 * Never writes directly into the live data directory: candidates are staged,
 * validated, published by rename, then inversion-proved while a token-owned
 * sibling lock serializes every observation and mutation of that data
 * directory (dead or abandoned owners are recovered race-safely; live owners
 * are only ever waited on, bounded by `lockAcquireTimeoutMs`). Successful
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
	const publishStaging = options.publishStaging ?? rename;
	const backupLive = options.backupLive ?? rename;
	const releaseLock = options.releaseLock ?? releaseDataDirectoryLock;
	const lockAcquireTimeoutMs = options.lockAcquireTimeoutMs ?? DEFAULT_LOCK_ACQUIRE_TIMEOUT_MS;
	if (!Number.isInteger(lockAcquireTimeoutMs) || lockAcquireTimeoutMs <= 0) {
		throw new Error(
			`lockAcquireTimeoutMs must be a finite positive integer of milliseconds; received ${String(options.lockAcquireTimeoutMs)}`,
		);
	}

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
		expectedBodies.set(provider, encodeProviderModels(provider, models));
	}

	const lock = await acquireDataDirectoryLock(dataDir, lockAcquireTimeoutMs, options.onLockWait);
	let primaryFailure: unknown = undefined;
	let hasPrimaryFailure = false;
	try {
		const hadLive = await pathExists(dataDir);
		const manifestPath = join(dataDir, DATA_MANIFEST_FILE);
		const previousManifest = await pathExists(manifestPath)
			? await Bun.file(manifestPath).bytes()
			: null;
		const manifestBody = previousManifest === null
			? null
			: rebuildProviderManifest(catalog, expectedBodies, previousManifest);
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
			if (manifestBody !== null) {
				await writeFile(join(stagingDir, DATA_MANIFEST_FILE), manifestBody);
			}

			await validateStagingDirectory(stagingDir, wrappers, catalog, manifestBody);

			if (hadLive) {
				const candidate = uniqueSibling(dataDir, "backup");
				try {
					await backupLive(dataDir, candidate);
					backupDir = candidate;
				} catch (error) {
					const detail = error instanceof Error ? error.message : String(error);
					throw new Error(`failed to rename live data to backup: ${detail}`);
				}
			}

			try {
				await publishStaging(stagingDir, dataDir);
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
				let restoreFailure: string | null = null;
				try {
					if (published) {
						await removeIfExists(dataDir);
						if (backupDir !== null) {
							await rename(backupDir, dataDir);
							backupDir = null;
						}
					} else if (hadLive && backupDir !== null) {
						if (await pathExists(dataDir)) {
							restoreFailure = `live data path ${dataDir} is unexpectedly occupied`;
						} else {
							await rename(backupDir, dataDir);
							backupDir = null;
						}
					}
				} catch (restoreError) {
					restoreFailure = errorDetail(restoreError);
				}

				if (stagingPending) {
					await removeIfExists(stagingDir);
				}
				if (restoreFailure !== null) {
					const preservedBackup =
						backupDir === null ? "" : `; preserved known-good backup at ${backupDir}`;
					throw new Error(
						`reconstruction failed (${errorDetail(error)}); additionally failed to restore live data: ${restoreFailure}${preservedBackup}`,
					);
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
	} catch (error) {
		hasPrimaryFailure = true;
		primaryFailure = error;
		throw error;
	} finally {
		// Token-verified release: runs even after publish/proof/cleanup failures
		// above and removes only directories still owned by this acquisition.
		try {
			await releaseLock(lock);
		} catch (releaseError) {
			if (!hasPrimaryFailure) throw releaseError;
			throw new Error(
				`reconstruction failed (${errorDetail(primaryFailure)}); additionally failed to release reconstruction lock: ${errorDetail(releaseError)}`,
				{ cause: primaryFailure },
			);
		}
	}
}

async function main(): Promise<void> {
	await reconstructProviderData();
}

if (import.meta.main) {
	await main();
}
