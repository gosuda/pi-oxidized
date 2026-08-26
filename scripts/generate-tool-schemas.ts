#!/usr/bin/env bun
/**
 * Deterministic generator for the Phase 3 tool JSON-Schema fixtures (plan check 5).
 *
 * Source of truth: the checked-in reference tool registry at
 * `.references/pi/packages/coding-agent/src/core/tools/index.ts`. The reference
 * factories are executed under Bun and each ToolDefinition's TypeBox `parameters`
 * schema is dumped as JSON for the seven portable built-in tools
 * (read, bash, edit, write, grep, find, ls). The generator selects those seven
 * from the canonical registry, fails when any required tool is absent, and
 * tolerates reference-only platform tools such as `powershell`. Output lands in
 * `.agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/<tool>.json`.
 *
 * The TypeBox version determines the emitted JSON, so bare `typebox` imports are
 * pinned via a Bun resolve plugin to the exact version declared by the reference
 * authorities: `dependencies.typebox` in
 * `.references/pi/packages/coding-agent/package.json` and the matching
 * `node_modules/typebox` entry in `.references/pi/package-lock.json` must agree.
 * The pinned package is resolved from the reference's own node_modules when
 * present, otherwise from Bun's global install cache; in both cases the resolved
 * package.json version is verified exactly. When the pinned package cannot be
 * found the generator exits nonzero with an explicit install prerequisite and
 * leaves any prior fixtures untouched. There is no hand-authored schema fallback
 * and no hidden network mutation: any missing reference import, pin mismatch, or
 * malformed schema is a hard error.
 *
 * Documented normalization (the same normalization the Rust contract test applies
 * to schemars output before comparing):
 *   1. JSON round-trip drops non-JSON values (TypeBox symbol keys, undefined).
 *   2. The keys `$schema`, `title`, and `format` are removed at every object
 *      depth — nondeterministic/serializer-specific metadata. Semantic
 *      constraints (`description`, `type`, `properties`, `required`, `items`,
 *      union members, defaults, `additionalProperties`) are preserved verbatim.
 *   3. Object keys are sorted recursively for byte-stable emission; array order
 *      (e.g. `required`, `anyOf`) is preserved.
 *   4. Encoding is 2-space-indented JSON with a trailing newline.
 *
 * Usage: bun run scripts/generate-tool-schemas.ts
 */

import { constants as fsConstants } from "node:fs";
import { access, mkdir, readdir, readFile, rename, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(SCRIPT_DIR, "..");
const REFERENCE_PACKAGE_MANIFEST = join(REPO_ROOT, ".references/pi/packages/coding-agent/package.json");
const REFERENCE_PACKAGE_LOCK = join(REPO_ROOT, ".references/pi/package-lock.json");
const REFERENCE_TOOLS_INDEX = join(REPO_ROOT, ".references/pi/packages/coding-agent/src/core/tools/index.ts");
const REFERENCE_TYPEBOX_DIRS = [
	join(REPO_ROOT, ".references/pi/packages/coding-agent/node_modules/typebox"),
	join(REPO_ROOT, ".references/pi/node_modules/typebox"),
] as const;
const OUTPUT_DIR = join(REPO_ROOT, ".agent-tasks/pi-rust-rewrite/fixtures/tool-schemas");

/** The Rust surface owns these portable tools even when the reference adds platform-only tools. */
export const REQUIRED_TOOL_NAMES = ["read", "bash", "edit", "write", "grep", "find", "ls"] as const;

/** Nondeterministic metadata keys stripped by the documented normalization. */
const STRIPPED_METADATA_KEYS: Record<string, true> = {
	$schema: true,
	title: true,
	format: true,
};

/**
 * Neutral cwd handed to the reference factories. Tool schemas never embed the
 * cwd; factories only close over it for execution, which fixtures never run.
 */
const FIXTURE_CWD = "/pi-fixture";

interface BunResolveBuilder {
	onResolve(options: { filter: RegExp }, callback: () => { path: string }): void;
}

interface BunPluginHost {
	plugin(definition: { name: string; setup(build: BunResolveBuilder): void }): void;
}

type CreateAllToolDefinitions = (cwd: string) => Record<string, unknown>;

interface ToolRegistryModule {
	createAllToolDefinitions: CreateAllToolDefinitions;
}

function fail(message: string): never {
	console.error(message);
	process.exit(1);
}

function assertBunRuntime(): void {
	// Bun injects the global `Bun` object; Node/other runtimes do not.
	if (!("Bun" in globalThis) || globalThis.Bun === undefined) {
		fail(
			"missing prerequisite: Bun runtime required (run with `bun run scripts/generate-tool-schemas.ts`)",
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

function isToolRegistryModule(value: unknown): value is ToolRegistryModule {
	return isPlainObject(value) && typeof value.createAllToolDefinitions === "function";
}

async function readJsonFile(path: string, label: string): Promise<Record<string, unknown>> {
	await assertPathReadable(path, label);
	let parsed: unknown;
	try {
		parsed = JSON.parse(await readFile(path, "utf8"));
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		fail(`missing prerequisite: ${label} does not parse: ${detail}`);
	}
	if (!isPlainObject(parsed)) {
		fail(`missing prerequisite: ${label} root is not an object`);
	}
	return parsed;
}

/**
 * Read the authoritative typebox pin: the coding-agent manifest's exact
 * `dependencies.typebox` version, cross-checked against the root lockfile's
 * `node_modules/typebox` entry. Any disagreement or non-exact range is fatal.
 */
async function readPinnedTypeboxVersion(): Promise<string> {
	const manifest = await readJsonFile(REFERENCE_PACKAGE_MANIFEST, "reference package manifest");
	if (!isPlainObject(manifest.dependencies)) {
		fail("missing prerequisite: reference package manifest has no dependencies object");
	}
	const pin = manifest.dependencies.typebox;
	if (typeof pin !== "string" || !/^\d+\.\d+\.\d+$/.test(pin)) {
		fail(
			`missing prerequisite: reference manifest must pin an exact typebox version, found ${JSON.stringify(pin)}`,
		);
	}
	const lock = await readJsonFile(REFERENCE_PACKAGE_LOCK, "reference package lockfile");
	if (!isPlainObject(lock.packages) || !isPlainObject(lock.packages["node_modules/typebox"])) {
		fail('missing prerequisite: reference lockfile has no "node_modules/typebox" entry');
	}
	const locked = lock.packages["node_modules/typebox"].version;
	if (locked !== pin) {
		fail(
			`missing prerequisite: reference authorities disagree on typebox (manifest ${pin}, lockfile ${JSON.stringify(locked)})`,
		);
	}
	return pin;
}

function bunCacheRoots(): string[] {
	const roots: string[] = [];
	const cacheOverride = process.env.BUN_INSTALL_CACHE_DIR;
	if (cacheOverride !== undefined && cacheOverride !== "") {
		roots.push(resolve(cacheOverride));
	}
	const xdgCache = process.env.XDG_CACHE_HOME;
	if (xdgCache !== undefined && xdgCache !== "") {
		roots.push(join(xdgCache, "bun/install/cache"));
	}
	roots.push(join(process.env.HOME ?? tmpdir(), ".bun/install/cache"));
	return roots;
}

async function verifiedTypeboxEntry(packageDir: string, pin: string): Promise<string | undefined> {
	try {
		await access(join(packageDir, "package.json"), fsConstants.R_OK);
	} catch {
		return undefined;
	}
	let parsed: unknown;
	try {
		parsed = JSON.parse(await readFile(join(packageDir, "package.json"), "utf8"));
	} catch {
		return undefined;
	}
	if (!isPlainObject(parsed) || parsed.version !== pin) {
		return undefined;
	}
	const entry = join(packageDir, "build/index.mjs");
	try {
		await access(entry, fsConstants.R_OK);
	} catch {
		return undefined;
	}
	return entry;
}

/**
 * Resolve the pinned typebox entry point from the reference's own node_modules
 * (nearest-first) or Bun's global install cache. Absence is a hard error: the
 * generator never installs or mutates anything itself.
 */
async function resolvePinnedTypeboxEntry(pin: string): Promise<string> {
	for (const packageDir of REFERENCE_TYPEBOX_DIRS) {
		const entry = await verifiedTypeboxEntry(packageDir, pin);
		if (entry !== undefined) {
			return entry;
		}
	}
	const prefix = `typebox@${pin}@@@`;
	for (const root of bunCacheRoots()) {
		let entries: string[];
		try {
			entries = await readdir(root);
		} catch {
			continue;
		}
		for (const hit of entries.filter((entry) => entry.startsWith(prefix)).sort()) {
			const entry = await verifiedTypeboxEntry(join(root, hit), pin);
			if (entry !== undefined) {
				return entry;
			}
		}
	}
	fail(
		`missing prerequisite: typebox@${pin} (pinned by ${REFERENCE_PACKAGE_MANIFEST} and ${REFERENCE_PACKAGE_LOCK}) not found in reference node_modules (${REFERENCE_TYPEBOX_DIRS.join(", ")}) or Bun install cache (${bunCacheRoots().join(", ")}); install the reference dependencies or warm the Bun cache with exactly typebox@${pin}, then re-run`,
	);
}

function registerTypeboxPin(typeboxEntry: string): void {
	const globalScope: { Bun?: BunPluginHost } = globalThis;
	if (globalScope.Bun === undefined) {
		fail("missing prerequisite: Bun plugin API unavailable");
	}
	globalScope.Bun.plugin({
		name: "pi-pin-typebox",
		setup(build) {
			build.onResolve({ filter: /^typebox$/ }, () => ({ path: typeboxEntry }));
		},
	});
}

/** Select only Rust-owned portable schemas and reject an incomplete reference registry. */
export function selectPortableToolParameters(
	definitions: Record<string, unknown>,
): Record<string, unknown> {
	const missing = REQUIRED_TOOL_NAMES.filter((name) => !Object.hasOwn(definitions, name));
	if (missing.length > 0) {
		throw new Error(
			`missing prerequisite: reference tool registry lacks required tools: ${missing.join(", ")}`,
		);
	}
	const parametersByTool: Record<string, unknown> = {};
	for (const name of REQUIRED_TOOL_NAMES) {
		const definition = definitions[name];
		if (!isPlainObject(definition) || definition.parameters === undefined) {
			throw new Error(`missing prerequisite: reference tool "${name}" has no parameters schema`);
		}
		parametersByTool[name] = definition.parameters;
	}
	return parametersByTool;
}

async function loadToolParameters(): Promise<Record<string, unknown>> {
	await assertPathReadable(REFERENCE_TOOLS_INDEX, "reference tool registry");
	let registry: unknown;
	try {
		// Dynamic import is required: the typebox-pin plugin must be registered
		// before the reference module graph loads, and static imports hoist above
		// Bun.plugin registration. The specifier is also a runtime-computed file URL.
		registry = await import(pathToFileURL(REFERENCE_TOOLS_INDEX).href);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		fail(
			`missing prerequisite: failed to import reference tool registry ${REFERENCE_TOOLS_INDEX}: ${detail}`,
		);
	}
	if (!isToolRegistryModule(registry)) {
		fail(
			`missing prerequisite: reference tool registry ${REFERENCE_TOOLS_INDEX} does not export createAllToolDefinitions`,
		);
	}
	const definitions = registry.createAllToolDefinitions(FIXTURE_CWD);
	if (!isPlainObject(definitions)) {
		fail("missing prerequisite: reference createAllToolDefinitions did not return a tool map");
	}
	try {
		return selectPortableToolParameters(definitions);
	} catch (error) {
		fail(error instanceof Error ? error.message : String(error));
	}
}

/**
 * Apply the documented normalization: strip `$schema`/`title`/`format` at every
 * object depth, recursively sort object keys, keep array order. Input must
 * already be plain JSON (the caller JSON round-trips first).
 */
function normalizeSchemaValue(value: unknown, strippedCounts: Record<string, number>): unknown {
	if (Array.isArray(value)) {
		return value.map((item) => normalizeSchemaValue(item, strippedCounts));
	}
	if (!isPlainObject(value)) {
		return value;
	}
	const normalized: Record<string, unknown> = {};
	for (const key of Object.keys(value).sort()) {
		if (STRIPPED_METADATA_KEYS[key] === true) {
			strippedCounts[key] = (strippedCounts[key] ?? 0) + 1;
			continue;
		}
		normalized[key] = normalizeSchemaValue(value[key], strippedCounts);
	}
	return normalized;
}

function validateToolSchema(name: string, schema: Record<string, unknown>): void {
	if (schema.type !== "object") {
		fail(`schema validation failed: tool "${name}" root is not a JSON object schema`);
	}
	if (!isPlainObject(schema.properties) || Object.keys(schema.properties).length === 0) {
		fail(`schema validation failed: tool "${name}" has no properties object`);
	}
	for (const [property, subschema] of Object.entries(schema.properties)) {
		if (!isPlainObject(subschema)) {
			fail(`schema validation failed: tool "${name}" property "${property}" is not a subschema object`);
		}
	}
	if (schema.required !== undefined) {
		if (!Array.isArray(schema.required) || schema.required.some((entry) => typeof entry !== "string")) {
			fail(`schema validation failed: tool "${name}" required is not a string array`);
		}
		for (const entry of schema.required) {
			if (!Object.hasOwn(schema.properties, entry)) {
				fail(
					`schema validation failed: tool "${name}" required entry ${JSON.stringify(entry)} missing from properties`,
				);
			}
		}
	}
}

function encodeSchema(schema: Record<string, unknown>): string {
	// 2-space indent + trailing newline, matching plan/JSON.stringify(_, null, 2).
	return `${JSON.stringify(schema, null, 2)}\n`;
}

function validateEncodedSchema(name: string, encoded: string): void {
	let parsed: unknown;
	try {
		parsed = JSON.parse(encoded);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		fail(`schema validation failed: tool "${name}" generated JSON does not parse: ${detail}`);
	}
	if (!isPlainObject(parsed)) {
		fail(`schema validation failed: tool "${name}" generated JSON root is not an object`);
	}
	// Re-encode must be byte-identical (determinism guard before write).
	if (encodeSchema(parsed) !== encoded) {
		fail(`schema validation failed: tool "${name}" generated JSON is not stable under re-encode`);
	}
}

/** Build every fixture body up front so a failure leaves prior fixtures untouched. */
function buildEncodedSchemas(parametersByTool: Record<string, unknown>): {
	encodedByTool: Record<string, string>;
	strippedTotals: Record<string, number>;
} {
	const encodedByTool: Record<string, string> = {};
	const strippedTotals: Record<string, number> = {};
	for (const name of REQUIRED_TOOL_NAMES) {
		// JSON round-trip drops TypeBox symbol keys and non-JSON values.
		const jsonValue: unknown = JSON.parse(JSON.stringify(parametersByTool[name]));
		if (!isPlainObject(jsonValue)) {
			fail(`schema validation failed: tool "${name}" parameters are not a JSON object`);
		}
		const strippedCounts: Record<string, number> = {};
		const normalized = normalizeSchemaValue(jsonValue, strippedCounts);
		if (!isPlainObject(normalized)) {
			fail(`schema validation failed: tool "${name}" normalization did not produce an object`);
		}
		validateToolSchema(name, normalized);
		const encoded = encodeSchema(normalized);
		validateEncodedSchema(name, encoded);
		encodedByTool[name] = encoded;
		for (const [key, count] of Object.entries(strippedCounts)) {
			strippedTotals[key] = (strippedTotals[key] ?? 0) + count;
		}
	}
	return { encodedByTool, strippedTotals };
}

async function writeAtomically(path: string, contents: string, counter: number): Promise<void> {
	const tempPath = join(dirname(path), `.tool-schemas.${process.pid}.${counter}.tmp.json`);
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
		fail(`failed to write schema atomically to ${path}: ${detail}`);
	}
}

async function main(): Promise<void> {
	assertBunRuntime();
	const pin = await readPinnedTypeboxVersion();
	const typeboxEntry = await resolvePinnedTypeboxEntry(pin);
	registerTypeboxPin(typeboxEntry);

	const parametersByTool = await loadToolParameters();
	const { encodedByTool, strippedTotals } = buildEncodedSchemas(parametersByTool);

	await mkdir(OUTPUT_DIR, { recursive: true });
	const summaryLines: string[] = [];
	let counter = 0;
	for (const name of REQUIRED_TOOL_NAMES) {
		const encoded = encodedByTool[name];
		if (encoded === undefined) {
			fail(`internal error: missing encoded schema for ${name}`);
		}
		counter += 1;
		await writeAtomically(join(OUTPUT_DIR, `${name}.json`), encoded, counter);
		const parsed: unknown = JSON.parse(encoded);
		// validateEncodedSchema above guarantees this shape; narrow without a cast.
		const schema = isPlainObject(parsed) ? parsed : {};
		const properties = isPlainObject(schema.properties) ? schema.properties : {};
		const required = Array.isArray(schema.required)
			? schema.required.filter((entry): entry is string => typeof entry === "string")
			: undefined;
		summaryLines.push(
			`  ${name}.json: ${Buffer.byteLength(encoded, "utf8")} bytes, properties=${Object.keys(properties).length}, required=${required === undefined ? "(absent)" : `[${required.join(",")}]`}`,
		);
	}

	const strippedReport = Object.keys(strippedTotals)
		.sort()
		.map((key) => `${key}=${strippedTotals[key]}`)
	.join(" ");
	process.stdout.write(
		[
			`Wrote ${REQUIRED_TOOL_NAMES.length} tool schemas to ${OUTPUT_DIR}`,
			`typebox: ${pin} (${typeboxEntry})`,
			`source: ${REFERENCE_TOOLS_INDEX}`,
			...summaryLines,
			`normalization: stripped ${strippedReport === "" ? "nothing (no $schema/title/format keys present)" : strippedReport}; object keys sorted recursively; array order preserved`,
		].join("\n") + "\n",
	);
}

if (import.meta.main) await main();
