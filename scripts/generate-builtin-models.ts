#!/usr/bin/env bun
/**
 * Offline deterministic generator for crates/pi-ai/data/builtin-models.json.
 *
 * Source of truth: the checked-in reference catalog at
 * `.references/pi-2.0/packages/ai/src/models.generated.ts` (and the per-provider
 * `*.models.ts` files it re-exports). Network fetches are intentionally
 * forbidden; runtime never needs Bun.
 *
 * Usage: bun run scripts/generate-builtin-models.ts
 * Usage (freshness check): bun run scripts/generate-builtin-models.ts --check
 *
 * OFFLINE prep mode (`--offline-capture <dir>`): a second, independent input
 * path that prepares a builtin-models catalog from an already-captured,
 * provenance-pinned snapshot instead of the live canonical reference
 * checkout. `<dir>` must contain `public-catalog-provenance.json` (fields:
 * `sourceRoot`, `sourceSha`, `catalogSha256`, `providers`, `models`) and a
 * `public-catalog-capture/models.json` catalog file. The mode re-verifies
 * the capture's exact git source SHA via `readReferenceHead`, re-hashes the
 * catalog bytes against the pinned `catalogSha256`, cross-checks the
 * provenance-declared provider/model counts, and runs the parsed catalog
 * through the same provider-set and encoding normalization as the default
 * path. Output never touches `OUTPUT_PATH`: it lands under
 * `target/reference-prep/<sourceSha-short>/builtin-models.prep.json` plus a
 * `builtin-models.prep.manifest.json` sidecar. Default and `--check`
 * behavior are unaffected by this mode's presence; it never activates
 * unless `--offline-capture` is explicit on the command line, and it never
 * reaches the network or a second canonical registry.
 */

import { access, mkdir, readFile, rename, unlink, writeFile } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { createHash } from "node:crypto";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
	assertCanonicalReference,
	canonicalReferenceRoot,
	readReferenceHead,
} from "./reference-identity.ts";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(SCRIPT_DIR, "..");
const REFERENCE_MODELS_PATH = join(
	canonicalReferenceRoot(),
	"packages/ai/src/models.generated.ts",
);
const OUTPUT_PATH = join(REPO_ROOT, "crates/pi-ai/data/builtin-models.json");

/** Static provider set from models.generated.ts MODELS keys (sorted). */
export const EXPECTED_PROVIDER_IDS = [
	"amazon-bedrock",
	"ant-ling",
	"anthropic",
	"azure-openai-responses",
	"baseten",
	"cerebras",
	"cloudflare-ai-gateway",
	"cloudflare-workers-ai",
	"deepseek",
	"fireworks",
	"github-copilot",
	"google",
	"google-vertex",
	"groq",
	"huggingface",
	"kimi-coding",
	"minimax",
	"minimax-cn",
	"mistral",
	"moonshotai",
	"moonshotai-cn",
	"nvidia",
	"openai",
	"openai-codex",
	"opencode",
	"opencode-go",
	"openrouter",
	"qwen-token-plan",
	"qwen-token-plan-cn",
	"qwen-token-plan-individual",
	"together",
	"vercel-ai-gateway",
	"xai",
	"xiaomi",
	"xiaomi-token-plan-ams",
	"xiaomi-token-plan-cn",
	"xiaomi-token-plan-sgp",
	"zai",
	"zai-coding-cn",
] as const;

// ---------------------------------------------------------------------------
// OFFLINE prep mode constants
// ---------------------------------------------------------------------------

/** Provenance sidecar filename expected inside `--offline-capture <dir>`. */
const OFFLINE_PROVENANCE_FILENAME = "public-catalog-provenance.json";
/** Capture subdirectory name inside `--offline-capture <dir>`. */
const OFFLINE_CAPTURE_SUBDIR = "public-catalog-capture";
/** Frozen catalog filename inside the capture subdirectory. */
const OFFLINE_CATALOG_FILENAME = "models.json";
/** Prep output root, isolated from the canonical generated artifact tree. */
const OFFLINE_OUTPUT_ROOT = join(REPO_ROOT, "target/reference-prep");
/** Prep artifact filename, distinct from the canonical builtin-models.json. */
const OFFLINE_ARTIFACT_FILENAME = "builtin-models.prep.json";
/** Prep artifact provenance sidecar filename. */
const OFFLINE_MANIFEST_FILENAME = "builtin-models.prep.manifest.json";
/** Recorded in the prep manifest's `generator` field. */
const OFFLINE_GENERATOR_LABEL = "scripts/generate-builtin-models.ts --offline-capture";

const FULL_SHA_PATTERN = /^[0-9a-f]{40}$/;
const SHA256_HEX_PATTERN = /^[0-9a-f]{64}$/;

function fail(message: string): never {
	console.error(message);
	process.exit(1);
}

function assertBunRuntime(): void {
	// Bun injects the global `Bun` object; Node/other runtimes do not.
	if (!("Bun" in globalThis) || globalThis.Bun === undefined) {
		fail(
			`missing prerequisite: Bun runtime required (run with \`bun run scripts/generate-builtin-models.ts\`)`,
		);
	}
}

async function assertPathReadable(path: string, label: string): Promise<void> {
	try {
		await access(path, fsConstants.R_OK);
	} catch {
		fail(`missing prerequisite: ${label} not found or unreadable: ${path}`);
	}
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function sortRecordDeep(value: unknown): unknown {
	if (Array.isArray(value)) {
		return value.map(sortRecordDeep);
	}
	if (!isPlainObject(value)) {
		return value;
	}
	const sorted = Object.create(null) as Record<string, unknown>;
	for (const key of Object.keys(value).sort()) {
		const nested = value[key];
		sorted[key] = sortRecordDeep(nested);
	}
	return sorted;
}

function cloneJsonValue(value: unknown): unknown {
	// structuredClone preserves plain objects/arrays/primitives without dropping
	// unknown model fields; models are plain data (no functions/symbols).
	return structuredClone(value);
}

async function loadReferenceModels(): Promise<Record<string, Record<string, unknown>>> {
	await assertPathReadable(REFERENCE_MODELS_PATH, "reference catalog export");

	let imported: unknown;
	try {
		imported = await import(pathToFileURL(REFERENCE_MODELS_PATH).href);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		fail(
			`missing prerequisite: failed to import reference catalog export ${REFERENCE_MODELS_PATH}: ${detail}`,
		);
	}

	if (!isPlainObject(imported) || !("MODELS" in imported)) {
		fail(
			`missing prerequisite: reference catalog export ${REFERENCE_MODELS_PATH} does not export MODELS object`,
		);
	}

	const models = imported.MODELS;
	if (!isPlainObject(models)) {
		fail(
			`missing prerequisite: reference catalog export ${REFERENCE_MODELS_PATH} does not export MODELS object`,
		);
	}

	const catalog = Object.create(null) as Record<string, Record<string, unknown>>;
	for (const [providerId, providerModels] of Object.entries(models)) {
		if (!isPlainObject(providerModels)) {
			fail(
				`missing prerequisite: reference provider "${providerId}" is not a model map`,
			);
		}
		const providerCatalog = Object.create(null) as Record<string, unknown>;
		for (const [modelId, model] of Object.entries(providerModels)) {
			if (!isPlainObject(model)) {
				fail(
					`missing prerequisite: reference model "${providerId}/${modelId}" is not an object`,
				);
			}
			// Preserve every field from the reference representation, including unknowns.
			providerCatalog[modelId] = cloneJsonValue(model);
		}
		catalog[providerId] = providerCatalog;
	}
	return catalog;
}

function validateProviderSet(catalog: Record<string, Record<string, unknown>>): void {
	const actual = Object.keys(catalog).sort();
	const expected = [...EXPECTED_PROVIDER_IDS];
	if (actual.length === 0) {
		fail("missing prerequisite: reference catalog export has zero providers");
	}

	const missing = expected.filter((id) => !Object.hasOwn(catalog, id));
	const unexpected = actual.filter(
		(id) => !(EXPECTED_PROVIDER_IDS as readonly string[]).includes(id),
	);

	if (missing.length > 0 || unexpected.length > 0) {
		const parts: string[] = [];
		if (missing.length > 0) {
			parts.push(`missing providers: ${missing.join(", ")}`);
		}
		if (unexpected.length > 0) {
			parts.push(`unexpected providers: ${unexpected.join(", ")}`);
		}
		fail(
			`missing prerequisite: reference provider set mismatch (${parts.join("; ")}); expected ${expected.length} providers from models.generated.ts`,
		);
	}

	for (const providerId of expected) {
		const models = catalog[providerId];
		if (models === undefined || Object.keys(models).length === 0) {
			fail(
				`missing prerequisite: reference provider "${providerId}" has zero models`,
			);
		}
	}
}

export function buildSortedCatalog(
	catalog: Record<string, Record<string, unknown>>,
): Record<string, Record<string, unknown>> {
	const sorted = Object.create(null) as Record<string, Record<string, unknown>>;
	for (const providerId of Object.keys(catalog).sort()) {
		const models = catalog[providerId];
		if (models === undefined) {
			fail(`missing prerequisite: reference provider "${providerId}" disappeared during sort`);
		}
		const providerModels = Object.create(null) as Record<string, unknown>;
		for (const modelId of Object.keys(models).sort()) {
			const model = models[modelId];
			if (model === undefined) {
				fail(
					`missing prerequisite: reference model "${providerId}/${modelId}" disappeared during sort`,
				);
			}
			// Sort object keys inside each model so JSON emission is byte-stable.
			const sortedModel = sortRecordDeep(model);
			if (!isPlainObject(sortedModel)) {
				fail(
					`missing prerequisite: reference model "${providerId}/${modelId}" is not an object after sort`,
				);
			}
			providerModels[modelId] = sortedModel;
		}
		sorted[providerId] = providerModels;
	}
	return sorted;
}

export function encodeCatalog(catalog: Record<string, Record<string, unknown>>): string {
	// 2-space indent + trailing newline, matching plan/JSON.stringify(_, null, 2).
	return `${JSON.stringify(catalog, null, 2)}\n`;
}

function validateEncodedCatalog(
	encoded: string,
	catalog: Record<string, Record<string, unknown>>,
): void {
	let parsed: unknown;
	try {
		parsed = JSON.parse(encoded);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		fail(`catalog validation failed: generated JSON does not parse: ${detail}`);
	}
	if (!isPlainObject(parsed)) {
		fail("catalog validation failed: generated JSON root is not an object");
	}

	const parsedProviders = Object.keys(parsed).sort();
	const expectedProviders = Object.keys(catalog).sort();
	if (parsedProviders.join("\0") !== expectedProviders.join("\0")) {
		fail("catalog validation failed: provider ID set diverged after JSON encode");
	}

	for (const providerId of expectedProviders) {
		const expectedModels = catalog[providerId];
		const actualModels = parsed[providerId];
		if (expectedModels === undefined || !isPlainObject(actualModels)) {
			fail(
				`catalog validation failed: provider "${providerId}" missing or invalid after encode`,
			);
		}
		const expectedIds = Object.keys(expectedModels).sort();
		const actualIds = Object.keys(actualModels).sort();
		if (expectedIds.length !== actualIds.length) {
			fail(
				`catalog validation failed: provider "${providerId}" model count ${actualIds.length} != ${expectedIds.length}`,
			);
		}
		if (expectedIds.join("\0") !== actualIds.join("\0")) {
			fail(
				`catalog validation failed: provider "${providerId}" model ID set diverged after encode`,
			);
		}
	}

	// Re-encode must be byte-identical (determinism guard before write).
	// `parsed` is already narrowed as a plain object; rebuild a typed catalog map.
	const reparseCatalog = Object.create(null) as Record<string, Record<string, unknown>>;
	for (const providerId of parsedProviders) {
		const providerModels = parsed[providerId];
		if (!isPlainObject(providerModels)) {
			fail(
				`catalog validation failed: provider "${providerId}" missing or invalid after encode`,
			);
		}
		reparseCatalog[providerId] = providerModels;
	}
	const reencoded = encodeCatalog(reparseCatalog);
	if (reencoded !== encoded) {
		fail("catalog validation failed: generated JSON is not stable under re-encode");
	}
}

async function writeAtomically(path: string, contents: string): Promise<void> {
	const dir = dirname(path);
	await mkdir(dir, { recursive: true });
	const tempPath = join(
		dir,
		`.builtin-models.${process.pid}.${Date.now()}.tmp.json`,
	);
	try {
		await writeFile(tempPath, contents, { encoding: "utf8" });
		await rename(tempPath, path);
	} catch (error) {
		try {
			await unlink(tempPath);
		} catch {
			// best-effort temp cleanup
		}
		const detail = error instanceof Error ? error.message : String(error);
		fail(`failed to write catalog atomically to ${path}: ${detail}`);
	}
}

function catalogTotals(catalog: Record<string, Record<string, unknown>>): {
	providerIds: string[];
	totalModels: number;
	lines: string[];
} {
	const providerIds = Object.keys(catalog).sort();
	let totalModels = 0;
	const lines: string[] = [];
	for (const providerId of providerIds) {
		const models = catalog[providerId];
		const count = models === undefined ? 0 : Object.keys(models).length;
		totalModels += count;
		lines.push(`  ${providerId}: ${count}`);
	}
	return { providerIds, totalModels, lines };
}

function summarize(catalog: Record<string, Record<string, unknown>>): string {
	const { providerIds, totalModels, lines } = catalogTotals(catalog);
	return [
		`Wrote ${OUTPUT_PATH}`,
		`providers: ${providerIds.length}`,
		`models: ${totalModels}`,
		`source: ${REFERENCE_MODELS_PATH}`,
		...lines,
	].join("\n");
}

function summarizeOffline(
	catalog: Record<string, Record<string, unknown>>,
	artifactPath: string,
	sourcePath: string,
): string {
	const { providerIds, totalModels, lines } = catalogTotals(catalog);
	return [
		`Wrote ${artifactPath}`,
		`providers: ${providerIds.length}`,
		`models: ${totalModels}`,
		`source: ${sourcePath}`,
		...lines,
	].join("\n");
}

// ---------------------------------------------------------------------------
// OFFLINE prep mode
// ---------------------------------------------------------------------------

interface OfflineProvenance {
	readonly schemaVersion: number;
	readonly sourceRoot: string;
	readonly sourceSha: string;
	readonly catalogSha256: string;
	readonly providers: number;
	readonly models: number;
}

/** Validates the subset of provenance fields this mode depends on; tolerates extra fields. */
function parseOfflineProvenance(raw: unknown, provenancePath: string): OfflineProvenance {
	if (!isPlainObject(raw)) {
		fail(`missing prerequisite: offline provenance ${provenancePath} is not a JSON object`);
	}
	const { schemaVersion, sourceRoot, sourceSha, catalogSha256, providers, models } = raw;
	if (typeof schemaVersion !== "number" || !Number.isInteger(schemaVersion)) {
		fail(
			`missing prerequisite: offline provenance ${provenancePath} field "schemaVersion" is not an integer`,
		);
	}
	if (typeof sourceRoot !== "string" || sourceRoot.length === 0) {
		fail(
			`missing prerequisite: offline provenance ${provenancePath} field "sourceRoot" is not a non-empty string`,
		);
	}
	if (typeof sourceSha !== "string" || !FULL_SHA_PATTERN.test(sourceSha)) {
		fail(
			`missing prerequisite: offline provenance ${provenancePath} field "sourceSha" is not a full 40-hex-character SHA`,
		);
	}
	if (typeof catalogSha256 !== "string" || !SHA256_HEX_PATTERN.test(catalogSha256)) {
		fail(
			`missing prerequisite: offline provenance ${provenancePath} field "catalogSha256" is not a 64-hex-character SHA-256`,
		);
	}
	if (typeof providers !== "number" || !Number.isInteger(providers) || providers <= 0) {
		fail(
			`missing prerequisite: offline provenance ${provenancePath} field "providers" is not a positive integer`,
		);
	}
	if (typeof models !== "number" || !Number.isInteger(models) || models <= 0) {
		fail(
			`missing prerequisite: offline provenance ${provenancePath} field "models" is not a positive integer`,
		);
	}
	return { schemaVersion, sourceRoot, sourceSha, catalogSha256, providers, models };
}

async function loadOfflineProvenance(provenancePath: string): Promise<OfflineProvenance> {
	await assertPathReadable(provenancePath, "offline capture provenance");
	const raw = await readFile(provenancePath, "utf8");
	let parsed: unknown;
	try {
		parsed = JSON.parse(raw);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		fail(`missing prerequisite: offline provenance ${provenancePath} is not valid JSON: ${detail}`);
	}
	return parseOfflineProvenance(parsed, provenancePath);
}

/** Fail-closed gate: the capture's recorded source commit must equal the live checkout HEAD. */
function assertOfflineSourceSha(provenance: OfflineProvenance, provenancePath: string): void {
	const referenceRoot = resolve(REPO_ROOT, provenance.sourceRoot);
	let head: string;
	try {
		head = readReferenceHead(referenceRoot);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		fail(
			`offline capture source unreadable: ${provenancePath} sourceRoot "${provenance.sourceRoot}" (${referenceRoot}): ${detail}`,
		);
	}
	if (head !== provenance.sourceSha) {
		fail(
			`offline capture source mismatch: ${provenancePath} sourceSha ${provenance.sourceSha} != live HEAD ${head} at ${referenceRoot}`,
		);
	}
}

async function loadFrozenCapture(
	catalogPath: string,
	provenance: OfflineProvenance,
	provenancePath: string,
): Promise<Record<string, Record<string, unknown>>> {
	await assertPathReadable(catalogPath, "offline capture catalog");
	const raw = await readFile(catalogPath, "utf8");
	const actualSha256 = createHash("sha256").update(raw, "utf8").digest("hex");
	if (actualSha256 !== provenance.catalogSha256) {
		fail(
			`offline capture catalog hash mismatch: ${catalogPath} sha256 ${actualSha256} != provenance ${provenancePath} catalogSha256 ${provenance.catalogSha256}`,
		);
	}

	let parsed: unknown;
	try {
		parsed = JSON.parse(raw);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		fail(`offline capture catalog ${catalogPath} is not valid JSON: ${detail}`);
	}
	if (!isPlainObject(parsed)) {
		fail(`offline capture catalog ${catalogPath} root is not an object`);
	}

	const catalog = Object.create(null) as Record<string, Record<string, unknown>>;
	let modelCount = 0;
	for (const [providerId, providerModels] of Object.entries(parsed)) {
		if (!isPlainObject(providerModels)) {
			fail(`offline capture catalog ${catalogPath} provider "${providerId}" is not a model map`);
		}
		const providerCatalog = Object.create(null) as Record<string, unknown>;
		for (const [modelId, model] of Object.entries(providerModels)) {
			if (!isPlainObject(model)) {
				fail(
					`offline capture catalog ${catalogPath} model "${providerId}/${modelId}" is not an object`,
				);
			}
			providerCatalog[modelId] = cloneJsonValue(model);
			modelCount += 1;
		}
		catalog[providerId] = providerCatalog;
	}

	const providerCount = Object.keys(catalog).length;
	if (providerCount !== provenance.providers) {
		fail(
			`offline capture provider count mismatch: ${catalogPath} has ${providerCount} providers != provenance ${provenancePath} providers ${provenance.providers}`,
		);
	}
	if (modelCount !== provenance.models) {
		fail(
			`offline capture model count mismatch: ${catalogPath} has ${modelCount} models != provenance ${provenancePath} models ${provenance.models}`,
		);
	}

	return catalog;
}

/** Deterministic prep output locations, keyed off the verified source SHA. */
export function offlineOutputPaths(sourceSha: string): {
	readonly dir: string;
	readonly artifactPath: string;
	readonly manifestPath: string;
} {
	const dir = join(OFFLINE_OUTPUT_ROOT, sourceSha.slice(0, 8));
	return {
		dir,
		artifactPath: join(dir, OFFLINE_ARTIFACT_FILENAME),
		manifestPath: join(dir, OFFLINE_MANIFEST_FILENAME),
	};
}

function assertOfflineOutputIsolated(path: string): void {
	if (resolve(path) === resolve(OUTPUT_PATH)) {
		fail(`offline prep refuses to overwrite canonical catalog: ${path}`);
	}
}

async function runOfflinePrep(captureRootArg: string): Promise<void> {
	const captureRoot = resolve(REPO_ROOT, captureRootArg);
	const provenancePath = join(captureRoot, OFFLINE_PROVENANCE_FILENAME);
	const catalogPath = join(captureRoot, OFFLINE_CAPTURE_SUBDIR, OFFLINE_CATALOG_FILENAME);

	const provenance = await loadOfflineProvenance(provenancePath);
	assertOfflineSourceSha(provenance, provenancePath);
	const catalog = await loadFrozenCapture(catalogPath, provenance, provenancePath);
	validateProviderSet(catalog);
	const sorted = buildSortedCatalog(catalog);
	const encoded = encodeCatalog(sorted);
	validateEncodedCatalog(encoded, sorted);

	const { artifactPath, manifestPath } = offlineOutputPaths(provenance.sourceSha);
	assertOfflineOutputIsolated(artifactPath);
	assertOfflineOutputIsolated(manifestPath);

	await writeAtomically(artifactPath, encoded);
	const manifest = {
		generator: OFFLINE_GENERATOR_LABEL,
		sourceRoot: provenance.sourceRoot,
		sourceSha: provenance.sourceSha,
		outputSha256: createHash("sha256").update(encoded, "utf8").digest("hex"),
	};
	await writeAtomically(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);

	process.stdout.write(`${summarizeOffline(sorted, artifactPath, catalogPath)}\n`);
}

// ---------------------------------------------------------------------------
// CLI dispatch
// ---------------------------------------------------------------------------

type CliArgs =
	| { readonly mode: "default" }
	| { readonly mode: "check" }
	| { readonly mode: "offline"; readonly offlineCaptureDir: string };

function parseCliArgs(argv: readonly string[]): CliArgs {
	const offlineIndex = argv.indexOf("--offline-capture");
	const hasCheck = argv.includes("--check");
	if (offlineIndex !== -1) {
		if (hasCheck) {
			fail("usage: --offline-capture and --check are mutually exclusive");
		}
		const offlineCaptureDir = argv[offlineIndex + 1];
		if (offlineCaptureDir === undefined || offlineCaptureDir.startsWith("--")) {
			fail("usage: --offline-capture requires a directory argument");
		}
		return { mode: "offline", offlineCaptureDir };
	}
	return { mode: hasCheck ? "check" : "default" };
}

async function main(): Promise<void> {
	const args = parseCliArgs(process.argv);
	if (args.mode === "offline") {
		// Explicit non-default path: caller-owned gating via the capture's own
		// provenance-pinned source SHA, never the canonical B assertion below.
		assertBunRuntime();
		await runOfflinePrep(args.offlineCaptureDir);
		return;
	}

	// Fail closed before the reference catalog is imported or read.
	assertCanonicalReference();
	assertBunRuntime();
	const catalog = await loadReferenceModels();
	validateProviderSet(catalog);
	const sorted = buildSortedCatalog(catalog);
	const encoded = encodeCatalog(sorted);
	validateEncodedCatalog(encoded, sorted);
	if (args.mode === "check") {
		const onDisk = await readFile(OUTPUT_PATH, "utf8").catch(() => null);
		if (onDisk !== encoded) {
			process.stderr.write(`stale builtin-models catalog: ${OUTPUT_PATH}\n`);
			process.exit(1);
		}
		process.stdout.write(`BUILTIN_MODELS_FRESH ${OUTPUT_PATH}\n`);
		return;
	}
	await writeAtomically(OUTPUT_PATH, encoded);
	process.stdout.write(`${summarize(sorted)}\n`);
}

if (import.meta.main) {
	await main();
}
