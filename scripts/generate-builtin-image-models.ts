#!/usr/bin/env bun
/**
 * Offline deterministic generator for crates/pi-ai/data/builtin-image-models.json.
 *
 * Source of truth: the frozen native reference catalog at
 * `nativeReferenceRoot()/packages/ai/src/image-models.generated.ts` (the
 * `IMAGE_MODELS` export, shaped `{provider: {modelId: {camelCase model}}}`).
 * Network fetches are intentionally forbidden; runtime never needs Bun.
 *
 * Usage: bun run scripts/generate-builtin-image-models.ts
 * Usage (freshness check): bun run scripts/generate-builtin-image-models.ts --check
 *
 * The emitted JSON is the compiled-in catalog consumed by
 * `crates/pi-ai/src/images/mod.rs` through `include_str!` only, so this
 * generator mirrors the native `ImagesModel` decoder's fail-closed checks at
 * generation time: required non-empty string fields, non-empty modality
 * lists with lowercase `"image"`/`"text"` values, and catalog-key
 * consistency (`model.provider`/`model.id` must equal their nesting keys).
 * Provider and model keys (and every nested object key) are deep-sorted so
 * the emitted bytes are deterministic across runs; unknown model fields are
 * preserved verbatim (the native decoder keeps them in `extra`).
 */

import { access, mkdir, readFile, rename, unlink, writeFile } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
	assertNativeReference,
	nativeReferenceRoot,
} from "./reference-identity.ts";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(SCRIPT_DIR, "..");
const REFERENCE_IMAGE_MODELS_PATH = join(
	nativeReferenceRoot(),
	"packages/ai/src/image-models.generated.ts",
);
const OUTPUT_PATH = join(REPO_ROOT, "crates/pi-ai/data/builtin-image-models.json");

/** Static image-provider set from image-models.generated.ts IMAGE_MODELS keys (sorted). */
export const EXPECTED_IMAGE_PROVIDER_IDS = ["openrouter"] as const;

/** Modality vocabularies enforced by the native ImagesModel decoder. */
const INPUT_MODALITIES: readonly string[] = ["image", "text"];
const OUTPUT_MODALITIES: readonly string[] = ["image", "text"];

/** Required non-empty string fields on every emitted image model (native decoder contract). */
const REQUIRED_STRING_FIELDS: readonly string[] = [
	"id",
	"name",
	"api",
	"provider",
	"baseUrl",
];

function fail(message: string): never {
	console.error(message);
	process.exit(1);
}

function assertBunRuntime(): void {
	// Bun injects the global `Bun` object; Node/other runtimes do not.
	if (!("Bun" in globalThis) || globalThis.Bun === undefined) {
		fail(
			`missing prerequisite: Bun runtime required (run with \`bun run scripts/generate-builtin-image-models.ts\`)`,
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

async function loadReferenceImageModels(): Promise<
	Record<string, Record<string, unknown>>
> {
	await assertPathReadable(REFERENCE_IMAGE_MODELS_PATH, "reference image catalog export");

	let imported: unknown;
	// Dynamic import is load-bearing: the frozen reference checkout lives
	// outside the package graph (the native reference root) and is selected at
	// runtime by nativeReferenceRoot(); a static specifier cannot reach it.
	try {
		imported = await import(pathToFileURL(REFERENCE_IMAGE_MODELS_PATH).href);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		fail(
			`missing prerequisite: failed to import reference image catalog export ${REFERENCE_IMAGE_MODELS_PATH}: ${detail}`,
		);
	}

	if (!isPlainObject(imported) || !("IMAGE_MODELS" in imported)) {
		fail(
			`missing prerequisite: reference image catalog export ${REFERENCE_IMAGE_MODELS_PATH} does not export IMAGE_MODELS object`,
		);
	}

	const models = imported.IMAGE_MODELS;
	if (!isPlainObject(models)) {
		fail(
			`missing prerequisite: reference image catalog export ${REFERENCE_IMAGE_MODELS_PATH} does not export IMAGE_MODELS object`,
		);
	}

	const catalog = Object.create(null) as Record<string, Record<string, unknown>>;
	for (const [providerId, providerModels] of Object.entries(models)) {
		if (!isPlainObject(providerModels)) {
			fail(
				`missing prerequisite: reference image provider "${providerId}" is not a model map`,
			);
		}
		const providerCatalog = Object.create(null) as Record<string, unknown>;
		for (const [modelId, model] of Object.entries(providerModels)) {
			if (!isPlainObject(model)) {
				fail(
					`missing prerequisite: reference image model "${providerId}/${modelId}" is not an object`,
				);
			}
			// Preserve every field from the reference representation, including unknowns.
			providerCatalog[modelId] = cloneJsonValue(model);
		}
		catalog[providerId] = providerCatalog;
	}
	return catalog;
}

/** Native-decoder twin: required fields, modalities, and catalog-key consistency. */
function validateImageModel(
	model: Record<string, unknown>,
	providerId: string,
	modelId: string,
): void {
	const modelPath = `${providerId}/${modelId}`;
	for (const field of REQUIRED_STRING_FIELDS) {
		const value = model[field];
		if (typeof value !== "string" || value.trim().length === 0) {
			fail(
				`missing prerequisite: reference image model "${modelPath}" field "${field}" is not a non-empty string`,
			);
		}
	}

	const providerField = model.provider;
	if (providerField !== providerId) {
		fail(
			`missing prerequisite: reference image model "${modelPath}" field "provider" (${String(providerField)}) does not match catalog key "${providerId}"`,
		);
	}
	const idField = model.id;
	if (idField !== modelId) {
		fail(
			`missing prerequisite: reference image model "${modelPath}" field "id" (${String(idField)}) does not match catalog key "${modelId}"`,
		);
	}

	const input = model.input;
	if (!Array.isArray(input) || input.length === 0) {
		fail(
			`missing prerequisite: reference image model "${modelPath}" field "input" is not a non-empty modality array`,
		);
	}
	for (const modality of input) {
		if (!INPUT_MODALITIES.includes(modality)) {
			fail(
				`missing prerequisite: reference image model "${modelPath}" has unknown input modality ${JSON.stringify(modality)}`,
			);
		}
	}

	const output = model.output;
	if (!Array.isArray(output) || output.length === 0) {
		fail(
			`missing prerequisite: reference image model "${modelPath}" field "output" is not a non-empty modality array`,
		);
	}
	for (const modality of output) {
		if (!OUTPUT_MODALITIES.includes(modality)) {
			fail(
				`missing prerequisite: reference image model "${modelPath}" has unknown output modality ${JSON.stringify(modality)}`,
			);
		}
	}

	const cost = model.cost;
	if (!isPlainObject(cost)) {
		fail(
			`missing prerequisite: reference image model "${modelPath}" field "cost" is not an object`,
		);
	}
	for (const [rate, value] of Object.entries(cost)) {
		if (typeof value !== "number" || !Number.isFinite(value)) {
			fail(
				`missing prerequisite: reference image model "${modelPath}" cost rate "${rate}" is not a finite number`,
			);
		}
	}
}

function validateImageProviderSet(
	catalog: Record<string, Record<string, unknown>>,
): void {
	const actual = Object.keys(catalog).sort();
	const expected = [...EXPECTED_IMAGE_PROVIDER_IDS];
	if (actual.length === 0) {
		fail("missing prerequisite: reference image catalog export has zero providers");
	}

	const missing = expected.filter((id) => !Object.hasOwn(catalog, id));
	const unexpected = actual.filter(
		(id) => !(EXPECTED_IMAGE_PROVIDER_IDS as readonly string[]).includes(id),
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
			`missing prerequisite: reference image provider set mismatch (${parts.join("; ")}); expected ${expected.length} providers from image-models.generated.ts`,
		);
	}

	for (const providerId of expected) {
		const models = catalog[providerId];
		if (models === undefined || Object.keys(models).length === 0) {
			fail(
				`missing prerequisite: reference image provider "${providerId}" has zero models`,
			);
		}
		for (const [modelId, model] of Object.entries(models)) {
			if (!isPlainObject(model)) {
				fail(
					`missing prerequisite: reference image model "${providerId}/${modelId}" is not an object`,
				);
			}
			validateImageModel(model, providerId, modelId);
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
			fail(`missing prerequisite: reference image provider "${providerId}" disappeared during sort`);
		}
		const providerModels = Object.create(null) as Record<string, unknown>;
		for (const modelId of Object.keys(models).sort()) {
			const model = models[modelId];
			if (model === undefined) {
				fail(
					`missing prerequisite: reference image model "${providerId}/${modelId}" disappeared during sort`,
				);
			}
			// Sort object keys inside each model so JSON emission is byte-stable.
			const sortedModel = sortRecordDeep(model);
			if (!isPlainObject(sortedModel)) {
				fail(
					`missing prerequisite: reference image model "${providerId}/${modelId}" is not an object after sort`,
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
		`.builtin-image-models.${process.pid}.${Date.now()}.tmp.json`,
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
		`source: ${REFERENCE_IMAGE_MODELS_PATH}`,
		...lines,
	].join("\n");
}

// ---------------------------------------------------------------------------
// CLI dispatch
// ---------------------------------------------------------------------------

type CliArgs = { readonly mode: "default" } | { readonly mode: "check" };

function parseCliArgs(argv: readonly string[]): CliArgs {
	return { mode: argv.includes("--check") ? "check" : "default" };
}

async function main(): Promise<void> {
	const args = parseCliArgs(process.argv);

	// Fail closed before the reference image catalog is imported or read.
	assertNativeReference();
	assertBunRuntime();
	const catalog = await loadReferenceImageModels();
	validateImageProviderSet(catalog);
	const sorted = buildSortedCatalog(catalog);
	const encoded = encodeCatalog(sorted);
	validateEncodedCatalog(encoded, sorted);
	if (args.mode === "check") {
		const onDisk = await readFile(OUTPUT_PATH, "utf8").catch(() => null);
		if (onDisk !== encoded) {
			process.stderr.write(`stale builtin-image-models catalog: ${OUTPUT_PATH}\n`);
			process.exit(1);
		}
		process.stdout.write(`BUILTIN_IMAGE_MODELS_FRESH ${OUTPUT_PATH}\n`);
		return;
	}
	await writeAtomically(OUTPUT_PATH, encoded);
	process.stdout.write(`${summarize(sorted)}\n`);
}

if (import.meta.main) {
	await main();
}
