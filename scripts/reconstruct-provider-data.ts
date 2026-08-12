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
const MANIFEST_SCHEMA_VERSION = 3;
// UTC time of the newest provider snapshot commit in pinned reference 4488ad55.
const PINNED_PROVIDER_DATA_GENERATED_AT = "2026-07-30T07:01:42.000Z";
const LOCK_OWNER_VERSION = 2;
/**
 * Heartbeat freshness contract: while an owner holds the lock it atomically
 * rewrites its owner record at this interval, and contenders classify a
 * record whose heartbeat (or creation, before the first beat) is older than
 * this bound as stale even when the recorded pid still exists — the pid may
 * have been recycled for an unrelated process, so pid liveness alone can wedge
 * recovery forever. PID death remains an immediate staleness signal.
 */
const LOCK_HEARTBEAT_INTERVAL_MS = 1_000;
const LOCK_HEARTBEAT_STALE_MS = 5_000;
type FileSystemError = Error & { code?: string };

export type ProviderCatalog = Record<string, Record<string, unknown>>;

export type ReconstructProviderDataOptions = {
	repoRoot?: string;
	catalogPath?: string;
	providersDir?: string;
	dataDir?: string;
	/**
	 * Timestamp for a new manifest when no live manifest exists. Repository
	 * defaults use the pinned package timestamp; custom paths omit the manifest
	 * unless the caller supplies this value.
	 */
	initialManifestGeneratedAt?: string;
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
	 * be a finite positive integer; defaults to 10_000. A live owner with a
	 * fresh heartbeat is never reaped — waiters simply time out at this bound.
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
	/**
	 * Optional override for the catalog snapshot restore in `defaultInversionProof`.
	 * Defaults to `writeFile` from `node:fs/promises`. Tests inject a failing
	 * restore here so the error-preservation path is exercised deterministically
	 * regardless of uid (chmod-based restore failures do not stop root).
	 */
	restoreCatalog?: (path: string, data: Uint8Array) => Promise<void>;
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
	/** Stops the owner heartbeat; awaited by release before any removal. */
	stopHeartbeat?: () => Promise<void>;
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
	previous: Uint8Array | null,
	initialGeneratedAt?: string,
): Uint8Array | null {
	let generatedAt = initialGeneratedAt;
	if (previous !== null) {
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
		const record = parsed as Record<string, unknown>;
		const previousGeneratedAt = record["generatedAt"];
		if (typeof previousGeneratedAt !== "string") {
			throw new Error("provider manifest has an invalid generation timestamp");
		}
		generatedAt = previousGeneratedAt;
		const previousSchemaVersion = record["schemaVersion"];
		if (previousSchemaVersion !== MANIFEST_SCHEMA_VERSION) {
			throw new Error(
				`provider manifest schemaVersion ${String(previousSchemaVersion)} is not supported; this reconstruction emits ${MANIFEST_SCHEMA_VERSION}`,
			);
		}
	}
	if (generatedAt === undefined) return null;
	if (Number.isNaN(Date.parse(generatedAt))) {
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
		schemaVersion: MANIFEST_SCHEMA_VERSION,
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
	heartbeatAtMs: number;
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
 * Milliseconds since the owner's most recent liveness proof: its first
 * heartbeat is its creation, later beats are the refreshes the owner writes
 * atomically while holding the lock.
 */
function lockHeartbeatAgeMs(record: LockOwnerRecord): number {
	return Math.max(0, Math.round(Date.now() - Math.max(record.heartbeatAtMs, record.createdAtMs)));
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
	if (
		Object.keys(candidate).sort().join(",") !==
		"createdAtMs,heartbeatAtMs,phase,pid,token,version"
	) {
		return null;
	}
	const { version, pid, token, createdAtMs, phase, heartbeatAtMs } = candidate;
	if (version !== LOCK_OWNER_VERSION) return null;
	if (typeof pid !== "number" || !Number.isSafeInteger(pid) || pid <= 0) return null;
	if (typeof token !== "string" || token.length === 0) return null;
	if (typeof createdAtMs !== "number" || !Number.isFinite(createdAtMs) || createdAtMs < 0) {
		return null;
	}
	if (phase !== "initializing" && phase !== "held") return null;
	if (
		typeof heartbeatAtMs !== "number" ||
		!Number.isFinite(heartbeatAtMs) ||
		heartbeatAtMs < 0
	) {
		return null;
	}
	return { version: LOCK_OWNER_VERSION, pid, token, createdAtMs, phase, heartbeatAtMs };
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
 * record whose pid is dead or whose heartbeat has gone stale, or invalid
 * metadata older than the initializing grace. A quarantine whose owner is
 * live and fresh is left untouched — it is the barrier that keeps claimants
 * provisional until that owner releases it.
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
		} else if (isProcessAlive(record.pid) && lockHeartbeatAgeMs(record) <= LOCK_HEARTBEAT_STALE_MS) {
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
 * Owner-side heartbeat: every {@link LOCK_HEARTBEAT_INTERVAL_MS} the holder
 * atomically rewrites its owner record (same-directory temp + rename) with a
 * fresh `heartbeatAtMs`, so contenders can distinguish a genuinely live owner
 * from a recycled pid. Before each refresh the stored identity is rechecked:
 * if the token or pid no longer matches (released, reaped, or transferred),
 * the heartbeat stops instead of ever writing into another owner's directory.
 * The timer is unref'd so an abandoned process cannot keep the runtime alive;
 * `stop` clears it and resolves only after the final in-flight refresh
 * settles, making cleanup ownership deterministic. Refresh failures never
 * throw — token-verified release is the only cleanup authority.
 */
function startLockHeartbeat(
	lockDir: string,
	record: LockOwnerRecord,
): () => Promise<void> {
	let inFlight: Promise<void> | null = null;
	const timer = setInterval(() => {
		if (inFlight !== null) return;
		inFlight = (async () => {
			const current = await readLockOwnerRecord(lockDir);
			if (
				current === null ||
				current.token !== record.token ||
				current.pid !== record.pid
			) {
				clearInterval(timer);
				return;
			}
			await writeLockOwnerRecord(lockDir, {
				...record,
				phase: "held",
				heartbeatAtMs: Date.now(),
			});
		})().catch(() => undefined);
	}, LOCK_HEARTBEAT_INTERVAL_MS);
	timer.unref?.();
	let stopped = false;
	return async () => {
		if (stopped) return;
		stopped = true;
		clearInterval(timer);
		const pending = inFlight;
		inFlight = null;
		if (pending !== null) await pending;
	};
}

/**
 * Complete a provisional claim on a freshly created lock directory. The
 * acquirer stays `initializing` until it proves that no reaper quarantine
 * exists and that the canonical metadata still carries its exact token; only
 * then does it promote to `held`, re-verify, and start the owner heartbeat
 * that keeps its liveness provable under PID reuse. Any failed check withdraws
 * the exact-token directories and reports the claim as lost, closing the
 * observe→rename ABA window from the claimant's side.
 */
async function claimLockDirectory(
	lockDir: string,
	token: string,
): Promise<(() => Promise<void>) | null> {
	const nowMs = Date.now();
	const record: LockOwnerRecord = {
		version: LOCK_OWNER_VERSION,
		pid: process.pid,
		token,
		createdAtMs: nowMs,
		phase: "initializing",
		heartbeatAtMs: nowMs,
	};
	try {
		await writeLockOwnerRecord(lockDir, record);
		if (!(await confirmProvisionalOwnership(lockDir, token))) {
			await removeOwnedLockDirectories(lockDir, token);
			return null;
		}
		await writeLockOwnerRecord(lockDir, { ...record, phase: "held" });
		if (!(await confirmProvisionalOwnership(lockDir, token))) {
			await removeOwnedLockDirectories(lockDir, token);
			return null;
		}
		return startLockHeartbeat(lockDir, record);
	} catch (error) {
		await removeOwnedLockDirectories(lockDir, token).catch(() => undefined);
		if (errorCode(error) === "ENOENT") return null;
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
	const heartbeatAgeMs = lockHeartbeatAgeMs(record);
	if (isProcessAlive(record.pid) && heartbeatAgeMs <= LOCK_HEARTBEAT_STALE_MS) {
		return {
			kind: "wait",
			detail: `live owner pid ${record.pid} (phase ${record.phase}, heartbeat ${heartbeatAgeMs}ms old); a fresh live owner is never reaped`,
		};
	}
	return {
		kind: "stale",
		observedToken: record.token,
		detail:
			heartbeatAgeMs > LOCK_HEARTBEAT_STALE_MS
				? `owner pid ${record.pid} (phase ${record.phase}) stopped heartbeating ${heartbeatAgeMs}ms ago (stale bound ${LOCK_HEARTBEAT_STALE_MS}ms); the pid may have been reused by an unrelated process`
				: `dead owner pid ${record.pid} (phase ${record.phase})`,
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
 * store versioned metadata and refresh it with a heartbeat while holding the
 * lock; an owner that is live AND fresh is never reaped, while dead owners,
 * stale-heartbeat owners (recycled pids), and abandoned-invalid locks are
 * recovered through the quarantine protocol in {@link recoverStaleLock}.
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
			const stopHeartbeat = await claimLockDirectory(lockDir, token);
			if (stopHeartbeat !== null) return { lockDir, token, stopHeartbeat };
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
 * Stop this handle's heartbeat first, then remove only directories that still
 * carry its exact token — the canonical lock directory and any quarantine
 * that captured it. Awaiting the heartbeat guarantees no owner refresh can
 * race the removals and no refresh can ever land on a successor's lock.
 * ENOENT or a different token means ownership was lost and removes nothing.
 */
export async function releaseDataDirectoryLock(handle: DataDirectoryLockHandle): Promise<void> {
	try {
		await handle.stopHeartbeat?.();
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
	const restoreCatalog = ctx.restoreCatalog ?? writeFile;
	let proofFailure: unknown;
	let restoreFailure: unknown;
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
	} finally {
		// Cleanup only — never throw inside finally (noUnsafeFinally).
		try {
			await restoreCatalog(ctx.catalogPath, before);
		} catch (restoreError) {
			restoreFailure = restoreError;
		}
	}
	// Compose after try/finally so the in-flight exception is never discarded.
	if (restoreFailure !== undefined) {
		if (proofFailure !== undefined) {
			throw new Error(
				`inversion proof failed (${errorDetail(proofFailure)}); additionally failed to restore catalog snapshot: ${errorDetail(restoreFailure)}`,
				{ cause: proofFailure },
			);
		}
		throw restoreFailure;
	}
	if (proofFailure !== undefined) {
		throw proofFailure;
	}
}

/**
 * Reconstruct provider data JSONs transactionally.
 *
 * Never writes directly into the live data directory: candidates are staged,
 * validated, published by rename, then inversion-proved while a token-owned
 * sibling lock serializes every observation and mutation of that data
 * directory (dead, heartbeat-stale, or abandoned owners are recovered
 * race-safely; live fresh owners are only ever waited on, bounded by
 * `lockAcquireTimeoutMs`). Successful
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
	let releaseFailure: unknown = undefined;
	let result!: ReconstructProviderDataResult;
	try {
		const hadLive = await pathExists(dataDir);
		const manifestPath = join(dataDir, DATA_MANIFEST_FILE);
		const previousManifest = await pathExists(manifestPath)
			? await Bun.file(manifestPath).bytes()
			: null;
		const initialManifestGeneratedAt = options.initialManifestGeneratedAt
			?? (usesRepositoryDefaultPaths(proofCtx) ? PINNED_PROVIDER_DATA_GENERATED_AT : undefined);
		const manifestBody = rebuildProviderManifest(
			catalog,
			expectedBodies,
			previousManifest,
			initialManifestGeneratedAt,
		);
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

		result = {
			written: wrappers.length,
			providers: wrappers,
			dataDir,
		};
	} catch (error) {
		hasPrimaryFailure = true;
		primaryFailure = error;
	} finally {
		// Token-verified release: runs even after publish/proof/cleanup failures
		// above and removes only directories still owned by this acquisition.
		// Cleanup only — never throw inside finally (noUnsafeFinally).
		try {
			await releaseLock(lock);
		} catch (releaseError) {
			releaseFailure = releaseError;
		}
	}
	// Compose after try/finally so the in-flight exception is never discarded.
	if (releaseFailure !== undefined) {
		if (hasPrimaryFailure) {
			throw new Error(
				`reconstruction failed (${errorDetail(primaryFailure)}); additionally failed to release reconstruction lock: ${errorDetail(releaseFailure)}`,
				{ cause: primaryFailure },
			);
		}
		throw releaseFailure;
	}
	if (hasPrimaryFailure) {
		throw primaryFailure;
	}
	return result;
}

async function main(): Promise<void> {
	await reconstructProviderData();
}

if (import.meta.main) {
	await main();
}
