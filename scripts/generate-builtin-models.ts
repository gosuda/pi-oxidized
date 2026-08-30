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
 */

import { access, mkdir, rename, writeFile } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
	assertCanonicalReference,
	canonicalReferenceRoot,
} from "./reference-identity.ts";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(SCRIPT_DIR, "..");
const REFERENCE_MODELS_PATH = join(
	canonicalReferenceRoot(),
	"packages/ai/src/models.generated.ts",
);
const OUTPUT_PATH = join(REPO_ROOT, "crates/pi-ai/data/builtin-models.json");

/** Static provider set from models.generated.ts MODELS keys (sorted). */
const EXPECTED_PROVIDER_IDS = [
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
			const { unlink } = await import("node:fs/promises");
			await unlink(tempPath);
		} catch {
			// best-effort temp cleanup
		}
		const detail = error instanceof Error ? error.message : String(error);
		fail(`failed to write catalog atomically to ${path}: ${detail}`);
	}
}

function summarize(catalog: Record<string, Record<string, unknown>>): string {
	const providerIds = Object.keys(catalog).sort();
	let totalModels = 0;
	const lines: string[] = [];
	for (const providerId of providerIds) {
		const models = catalog[providerId];
		const count = models === undefined ? 0 : Object.keys(models).length;
		totalModels += count;
		lines.push(`  ${providerId}: ${count}`);
	}
	return [
		`Wrote ${OUTPUT_PATH}`,
		`providers: ${providerIds.length}`,
		`models: ${totalModels}`,
		`source: ${REFERENCE_MODELS_PATH}`,
		...lines,
	].join("\n");
}

async function main(): Promise<void> {
	// Fail closed before the reference catalog is imported or read.
	assertCanonicalReference();
	assertBunRuntime();
	const catalog = await loadReferenceModels();
	validateProviderSet(catalog);
	const sorted = buildSortedCatalog(catalog);
	const encoded = encodeCatalog(sorted);
	validateEncodedCatalog(encoded, sorted);
	await writeAtomically(OUTPUT_PATH, encoded);
	process.stdout.write(`${summarize(sorted)}\n`);
}

if (import.meta.main) {
	await main();
}
