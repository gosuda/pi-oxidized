#!/usr/bin/env bun
/**
 * Pinned-version and compatibility-matrix doc generator (DOC-B, issue #139).
 *
 * Reads every registered pin constant from its authoritative source, cross-
 * asserts TS/Rust double-owner constants (PROTOCOL_VERSION, COMPATIBILITY_VERSION)
 * are equal, and emits a deterministic, machine-readable `docs/compatibility.md`.
 *
 * Two consecutive runs produce a byte-identical file.  Any disagreement between
 * a TS constant and its Rust mirror fails generation with a non-zero exit.
 *
 * Registered pin constants (consumed only through this generator):
 *   - workspace version  (Cargo.toml [workspace.package] + root package.json, cross-checked equal)
 *   - pi-tui-protocol package version
 *   - extension-host package version
 *   - PROTOCOL_VERSION        (TS: packages/pi-tui-protocol/src/types.ts  +  Rust: crates/pi-ext/src/protocol.rs)
 *   - COMPATIBILITY_VERSION   (TS: packages/pi-tui-protocol/src/types.ts  +  Rust: crates/pi-ext/src/protocol.rs)
 *   - BUN_RUNTIME_VERSION     (scripts/release/runtime.ts)
 *   - RELEASE_MANIFEST_SCHEMA (scripts/release/stage.ts)
 *   - engine floors: node >=22.19.0, bun >=1.3.0, rust-version from Cargo.toml
 *   - settled reference pin   (scripts/verification/docs-evidence.json referencePin)
 *   - compat-matrix.json row inventory
 */

import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

const REPO_ROOT = resolve(import.meta.dirname, "../..");

const PATHS = {
	cargoToml: "Cargo.toml",
	rootPkgJson: "package.json",
	piTuiProtocolPkgJson: "packages/pi-tui-protocol/package.json",
	extensionHostPkgJson: "packages/extension-host/package.json",
	tsProtocolTypes: "packages/pi-tui-protocol/src/types.ts",
	extHostVersion: "packages/extension-host/src/version.ts",
	rustProtocol: "crates/pi-ext/src/protocol.rs",
	bunRuntime: "scripts/release/runtime.ts",
	releaseStage: "scripts/release/stage.ts",
	docsEvidence: "scripts/verification/docs-evidence.json",
	compatMatrix: "scripts/verification/compat-matrix.json",
	output: "docs/compatibility.md",
} as const;


// ---------------------------------------------------------------------------
// Extraction helpers
// ---------------------------------------------------------------------------

function readFileText(relPath: string): string {
	const abs = resolve(REPO_ROOT, relPath);
	if (!existsSync(abs)) {
		throw new Error(`[generate-compat-docs] source file not found: ${relPath}`);
	}
	return readFileSync(abs, "utf8");
}

/** Extract `export const LABEL = "value"` or `export const LABEL = 1` from TS source. */
export function extractTsConst(content: string, label: string): string {
	const re = new RegExp(
		`(?:export\\s+)?(?:const|let|var)\\s+${escapeRegex(label)}\\s*=\\s*["']?([A-Za-z0-9_.+-]+)["']?`,
	);
	const m = content.match(re);
	if (!m || m[1] === undefined) {
		throw new Error(`[generate-compat-docs] TS constant ${label} not found`);
	}
	return m[1];
}

/** Extract `pub const LABEL: ... = value;` or `pub const LABEL = "value";` from Rust source. */
export function extractRustConst(content: string, label: string): string {
	const re = new RegExp(
		`pub\\s+const\\s+${escapeRegex(label)}\\s*(?::\\s*[^=]+)?\\s*=\\s*["']?([A-Za-z0-9_.+-]+)["']?\\s*;`,
	);
	const m = content.match(re);
	if (!m || m[1] === undefined) {
		throw new Error(`[generate-compat-docs] Rust constant ${label} not found`);
	}
	return m[1];
}

function escapeRegex(s: string): string {
	return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Extract a top-level `version = "x"` from the [workspace.package] section of Cargo.toml. */
export function extractCargoWorkspaceVersion(toml: string): string {
	// Match [workspace.package] block, then find version = "..."
	const blockRe = /\[workspace\.package\]/;
	const blockMatch = toml.match(blockRe);
	if (!blockMatch || blockMatch.index === undefined) {
		throw new Error("[generate-compat-docs] [workspace.package] section not found in Cargo.toml");
	}
	const afterBlock = toml.slice(blockMatch.index);
	// Stop at the next table header
	const nextTable = afterBlock.search(/\n\[/);
	const block = nextTable === -1 ? afterBlock : afterBlock.slice(0, nextTable);
	const versionRe = /^version\s*=\s*"([^"]+)"/m;
	const m = block.match(versionRe);
	if (!m || m[1] === undefined) {
		throw new Error("[generate-compat-docs] version not found in [workspace.package]");
	}
	return m[1];
}

/** Extract `rust-version = "x"` from the [workspace.package] section of Cargo.toml. */
export function extractRustVersion(toml: string): string {
	const blockRe = /\[workspace\.package\]/;
	const blockMatch = toml.match(blockRe);
	if (!blockMatch || blockMatch.index === undefined) {
		throw new Error("[generate-compat-docs] [workspace.package] section not found in Cargo.toml");
	}
	const afterBlock = toml.slice(blockMatch.index);
	const nextTable = afterBlock.search(/\n\[/);
	const block = nextTable === -1 ? afterBlock : afterBlock.slice(0, nextTable);
	const re = /^rust-version\s*=\s*"([^"]+)"/m;
	const m = block.match(re);
	if (!m || m[1] === undefined) {
		throw new Error("[generate-compat-docs] rust-version not found in [workspace.package]");
	}
	return m[1];
}

/** Extract a JSON field from a package.json file content. */
export function extractJsonField(jsonText: string, field: string): string {
	const obj = JSON.parse(jsonText) as Record<string, unknown>;
	const value = obj[field];
	if (typeof value !== "string") {
		throw new Error(`[generate-compat-docs] field ${field} not found or not a string in JSON`);
	}
	return value;
}

/** Extract an engines requirement from a package.json. */
export function extractEngineFloor(jsonText: string, engine: string): string {
	const obj = JSON.parse(jsonText) as Record<string, unknown>;
	const engines = obj["engines"];
	if (typeof engines !== "object" || engines === null) {
		throw new Error(`[generate-compat-docs] engines not found in JSON`);
	}
	const value = (engines as Record<string, unknown>)[engine];
	if (typeof value !== "string") {
		throw new Error(`[generate-compat-docs] engines.${engine} not found or not a string`);
	}
	return value;
}

// ---------------------------------------------------------------------------
// Pin collection
// ---------------------------------------------------------------------------

export interface CollectedPins {
	readonly workspaceVersion: string;
	readonly piTuiProtocolVersion: string;
	readonly extensionHostVersion: string;
	readonly tsProtocolVersion: string;
	readonly rustProtocolVersion: string;
	readonly tsCompatibilityVersion: string;
	readonly rustCompatibilityVersion: string;
	readonly extHostCompatibilityVersion: string;
	readonly bunRuntimeVersion: string;
	readonly releaseManifestSchema: string;
	readonly rustVersion: string;
	readonly nodeFloor: string;
	readonly bunFloor: string;
	readonly referencePin: string;
	readonly matrixVersion: string;
	readonly matrixRowCount: number;
	readonly matrixRows: ReadonlyArray<{ id: string; tier: string; surface: string; required: boolean }>;
}

/** Bare version number extracted from an engine floor string (e.g. "22.19.0" from ">=22.19.0"). */
function bareVersion(floor: string): string {
	const m = floor.match(/\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.]+)?/);
	return m && m[0] !== undefined ? m[0] : floor;
}

/** All semver-like literals that are registered pin values (for drift detection in the generated doc). */
export function registeredPinValues(pins: CollectedPins): readonly string[] {
	return [
		pins.workspaceVersion,
		pins.piTuiProtocolVersion,
		pins.extensionHostVersion,
		pins.tsCompatibilityVersion,
		pins.rustCompatibilityVersion,
		pins.extHostCompatibilityVersion,
		pins.bunRuntimeVersion,
		pins.releaseManifestSchema,
		pins.rustVersion,
		pins.nodeFloor,
		pins.bunFloor,
		pins.tsProtocolVersion,
		pins.rustProtocolVersion,
		bareVersion(pins.nodeFloor),
		bareVersion(pins.bunFloor),
		pins.matrixVersion,
	];
}

/**
 * Collect every registered pin from its authoritative source.
 * Cross-asserts TS/Rust double-owner constants are equal — throws on disagreement.
 */
export function collectPins(): CollectedPins {
	const cargoToml = readFileText(PATHS.cargoToml);
	const rootPkgJson = readFileText(PATHS.rootPkgJson);
	const piTuiProtocolPkgJson = readFileText(PATHS.piTuiProtocolPkgJson);
	const extensionHostPkgJson = readFileText(PATHS.extensionHostPkgJson);
	const tsProtocolTypes = readFileText(PATHS.tsProtocolTypes);
	const extHostVersionSrc = readFileText(PATHS.extHostVersion);
	const rustProtocol = readFileText(PATHS.rustProtocol);
	const bunRuntimeSrc = readFileText(PATHS.bunRuntime);
	const releaseStageSrc = readFileText(PATHS.releaseStage);
	const docsEvidenceJson = readFileText(PATHS.docsEvidence);
	const compatMatrixJson = readFileText(PATHS.compatMatrix);

	// Workspace version — cross-check Cargo.toml and root package.json are equal
	const cargoVersion = extractCargoWorkspaceVersion(cargoToml);
	const pkgVersion = extractJsonField(rootPkgJson, "version");
	if (cargoVersion !== pkgVersion) {
		throw new Error(
			`[generate-compat-docs] workspace version mismatch: Cargo.toml=${cargoVersion}, package.json=${pkgVersion}`,
		);
	}

	// TS/Rust double-owner constants — cross-assert equal
	const tsProtocolVersion = extractTsConst(tsProtocolTypes, "PROTOCOL_VERSION");
	const rustProtocolVersion = extractRustConst(rustProtocol, "PROTOCOL_VERSION");
	if (tsProtocolVersion !== rustProtocolVersion) {
		throw new Error(
			`[generate-compat-docs] PROTOCOL_VERSION mismatch: TS=${tsProtocolVersion}, Rust=${rustProtocolVersion}`,
		);
	}

	const tsCompatibilityVersion = extractTsConst(tsProtocolTypes, "COMPATIBILITY_VERSION");
	const rustCompatibilityVersion = extractRustConst(rustProtocol, "COMPATIBILITY_VERSION");
	if (tsCompatibilityVersion !== rustCompatibilityVersion) {
		throw new Error(
			`[generate-compat-docs] COMPATIBILITY_VERSION mismatch: TS=${tsCompatibilityVersion}, Rust=${rustCompatibilityVersion}`,
		);
	}

	// Extension-host also carries COMPATIBILITY_VERSION — cross-assert equal
	const extHostCompatibilityVersion = extractTsConst(extHostVersionSrc, "COMPATIBILITY_VERSION");
	if (tsCompatibilityVersion !== extHostCompatibilityVersion) {
		throw new Error(
			`[generate-compat-docs] COMPATIBILITY_VERSION mismatch: TS protocol=${tsCompatibilityVersion}, extension-host=${extHostCompatibilityVersion}`,
		);
	}

	// Engine floors
	const rustVersion = extractRustVersion(cargoToml);
	const nodeFloor = extractEngineFloor(piTuiProtocolPkgJson, "node");
	const bunFloor = extractEngineFloor(rootPkgJson, "bun");

	// Reference pin from docs-evidence ledger
	const docsEvidence = JSON.parse(docsEvidenceJson) as { referencePin: string };
	const referencePin = docsEvidence.referencePin;

	// Compat-matrix inventory
	const compatMatrix = JSON.parse(compatMatrixJson) as {
		version: string;
		rows: ReadonlyArray<{ id: string; tier: string; surface: string; required: boolean }>;
	};

	return {
		workspaceVersion: cargoVersion,
		piTuiProtocolVersion: extractJsonField(piTuiProtocolPkgJson, "version"),
		extensionHostVersion: extractJsonField(extensionHostPkgJson, "version"),
		tsProtocolVersion,
		rustProtocolVersion,
		tsCompatibilityVersion,
		rustCompatibilityVersion,
		extHostCompatibilityVersion,
		bunRuntimeVersion: extractTsConst(bunRuntimeSrc, "BUN_RUNTIME_VERSION"),
		releaseManifestSchema: extractTsConst(releaseStageSrc, "RELEASE_MANIFEST_SCHEMA"),
		rustVersion,
		nodeFloor,
		bunFloor,
		referencePin,
		matrixVersion: compatMatrix.version,
		matrixRowCount: compatMatrix.rows.length,
		matrixRows: compatMatrix.rows.map((r) => ({
			id: r.id,
			tier: r.tier,
			surface: r.surface,
			required: r.required,
		})),
	};
}

// ---------------------------------------------------------------------------
// Doc generation (deterministic, byte-stable)
// ---------------------------------------------------------------------------

/**
 * Generate the compatibility doc markdown from collected pins.
 * Output is deterministic: no timestamps, no random ordering, sorted where applicable.
 */
export function generateDoc(pins: CollectedPins): string {
	const lines: string[] = [];

	lines.push("# Compatibility Matrix");
	lines.push("");
	lines.push("<!--");
	lines.push("  This file is generated by scripts/verification/generate-compat-docs.ts (DOC-B, issue #139).");
	lines.push("  Do not edit by hand — rerun the generator after any pin constant changes.");
	lines.push("  Two consecutive runs produce a byte-identical file; CI fails on committed/generated drift.");
	lines.push("-->");
	lines.push("");

	// Workspace version
	lines.push("## Workspace Version");
	lines.push("");
	lines.push("| Source | Value |");
	lines.push("|--------|-------|");
	lines.push(`| \`Cargo.toml\` [workspace.package] | \`${pins.workspaceVersion}\` |`);
	lines.push(`| \`package.json\` (root) | \`${pins.workspaceVersion}\` |`);
	lines.push("");
	lines.push("Cross-checked equal between Cargo.toml and root package.json.");
	lines.push("");

	// Package versions
	lines.push("## Package Versions");
	lines.push("");
	lines.push("| Package | Version |");
	lines.push("|---------|---------|");
	lines.push(`| \`@earendil-works/pi-tui-protocol\` | \`${pins.piTuiProtocolVersion}\` |`);
	lines.push(`| \`@earendil-works/pi-extension-host\` | \`${pins.extensionHostVersion}\` |`);
	lines.push("");

	// Protocol and compatibility versions (double-owner cross-asserted)
	lines.push("## Protocol and Compatibility Versions");
	lines.push("");
	lines.push("TS, Rust, and extension-host triple-owner constants are cross-asserted equal by the generator.");
	lines.push("A disagreement fails generation.");
	lines.push("");
	lines.push("| Constant | TS source | Rust source | Extension-host source | Value |");
	lines.push("|----------|-----------|-------------|----------------------|-------|");
	lines.push(`| \`PROTOCOL_VERSION\` | \`packages/pi-tui-protocol/src/types.ts\` | \`crates/pi-ext/src/protocol.rs\` | — | \`${pins.tsProtocolVersion}\` |`);
	lines.push(`| \`COMPATIBILITY_VERSION\` | \`packages/pi-tui-protocol/src/types.ts\` | \`crates/pi-ext/src/protocol.rs\` | \`packages/extension-host/src/version.ts\` | \`${pins.tsCompatibilityVersion}\` |`);
	lines.push("");

	// Runtime and release constants
	lines.push("## Runtime and Release Constants");
	lines.push("");
	lines.push("| Constant | Source | Value |");
	lines.push("|----------|--------|-------|");
	lines.push(`| \`BUN_RUNTIME_VERSION\` | \`scripts/release/runtime.ts\` | \`${pins.bunRuntimeVersion}\` |`);
	lines.push(`| \`RELEASE_MANIFEST_SCHEMA\` | \`scripts/release/stage.ts\` | \`${pins.releaseManifestSchema}\` |`);
	lines.push("");

	// Engine floors
	lines.push("## Engine Floors");
	lines.push("");
	lines.push("| Engine | Floor | Source |");
	lines.push("|--------|-------|--------|");
	lines.push(`| Node | \`${pins.nodeFloor}\` | \`packages/pi-tui-protocol/package.json\` engines.node |`);
	lines.push(`| Bun | \`${pins.bunFloor}\` | \`package.json\` engines.bun |`);
	lines.push(`| Rust | \`>=${pins.rustVersion}\` | \`Cargo.toml\` [workspace.package] rust-version |`);
	lines.push("");

	// Reference pin
	lines.push("## Settled Reference Pin");
	lines.push("");
	lines.push("| Pin | Value | Source |");
	lines.push("|-----|-------|--------|");
	lines.push(`| Canonical reference SHA | \`${pins.referencePin}\` | \`scripts/verification/docs-evidence.json\` referencePin |`);
	lines.push("");

	// Machine-readable section
	lines.push("## Machine-Readable Compatibility Matrix");
	lines.push("");
	lines.push("<!-- compat-matrix.json row inventory as metadata -->");
	lines.push("```json");
	const matrixMeta = {
		schema: "pi.compatibility.matrix.v1",
		matrixVersion: pins.matrixVersion,
		rowCount: pins.matrixRowCount,
		rows: pins.matrixRows,
	};
	lines.push(JSON.stringify(matrixMeta, null, "\t"));
	lines.push("```");
	lines.push("");

	// Generated-block classification
	lines.push("## Generated-Block Classification");
	lines.push("");
	lines.push("The following surfaces classify as generated-block rows in the doc-evidence ledger:");
	lines.push("");
	lines.push("- Telemetry documentation (generated from source instrumentation)");
	lines.push("- Model catalogs (generated from provider data)");
	lines.push("- npm-shrinkwrap.json (generated from lockfile)");
	lines.push("- Compiled binaries/bundles (generated from build)");
	lines.push("");
	lines.push("The sync-docs safe-fix whitelist narrows to this generator's blocks plus registered");
	lines.push("badge/version lines.  Constants the dependency campaign will change are consumed only");
	lines.push("through version-pin ledger rows pointing at this generator.");
	lines.push("");

	return lines.join("\n");
}

// ---------------------------------------------------------------------------
// Drift detection
// ---------------------------------------------------------------------------

const SEMVER_RE = /\b\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.]+)?\b/g;

/**
 * Check 1: every semver literal in the generated doc must be a registered pin value.
 * Returns a list of drift findings (line — literal).
 */
export function findUnregisteredSemverInGeneratedDoc(
	pins: CollectedPins,
	docContent: string,
): readonly string[] {
	const findings: string[] = [];
	const registered = new Set(registeredPinValues(pins));
	const lines = docContent.split("\n");
	for (let i = 0; i < lines.length; i++) {
		const line = lines[i];
		if (line === undefined) continue;
		// Skip HTML comment markers and JSON code fence lines
		if (line.startsWith("<!--") || line.startsWith("```")) continue;
		const matches = line.match(SEMVER_RE);
		if (!matches) continue;
		for (const m of matches) {
			if (!registered.has(m)) {
				findings.push(`compatibility.md:${i + 1} — unregistered semver literal: ${m}`);
			}
		}
	}
	return findings;
}

/**
 * Check 2: no registered pin value appears in any other doc file.
 * Returns a list of drift findings (file:line — literal).
 */
export function findRegisteredPinsInOtherDocs(
	pins: CollectedPins,
	docsRoot: string,
): readonly string[] {
	const findings: string[] = [];
	const registered = new Set(
		registeredPinValues(pins).filter((v) => v.includes(".") || v.length >= 3),
	);
	const docsDir = resolve(docsRoot, "docs");
	if (!existsSync(docsDir)) return findings;
	scanDirForRegisteredPins(docsDir, registered, findings, docsDir);
	return findings;
}

function scanDirForRegisteredPins(
	dir: string,
	registered: Set<string>,
	findings: string[],
	root: string,
): void {
	for (const entry of readdirSync(dir)) {
		const fullPath = `${dir}/${entry}`;
		const stat = statSync(fullPath);
		if (stat.isDirectory()) {
			scanDirForRegisteredPins(fullPath, registered, findings, root);
			continue;
		}
		if (!entry.endsWith(".md") || entry === "compatibility.md") continue;

		const content = readFileSync(fullPath, "utf8");
		const lines = content.split("\n");
		const relPath = fullPath.slice(root.length + 1);
		let inFence = false;
		for (let i = 0; i < lines.length; i++) {
			const line = lines[i];
			if (line === undefined) continue;
			if (line.startsWith("```")) {
				inFence = !inFence;
				continue;
			}
			if (inFence || line.startsWith("<!--")) continue;

			for (const pin of registered) {
				if (line.includes(pin)) {
					findings.push(
						`${relPath}:${i + 1} — registered pin value found outside compatibility.md: ${pin}`,
					);
				}
			}
		}
	}
}

// ---------------------------------------------------------------------------
// CLI entrypoint
// ---------------------------------------------------------------------------

function main(): void {
	const check = process.argv.includes("--check");
	const pins = collectPins();
	const doc = generateDoc(pins);
	const outputPath = resolve(REPO_ROOT, PATHS.output);
	if (check) {
		let onDisk: string | null = null;
		try {
			onDisk = readFileSync(outputPath, "utf8");
		} catch {
			onDisk = null;
		}
		if (onDisk !== doc) {
			process.stderr.write(`[generate-compat-docs] stale committed doc: ${outputPath}\n`);
			process.exit(1);
		}
	} else {
		writeFileSync(outputPath, doc, "utf8");
	}

	// Check 1: every semver in the generated doc must be a registered pin
	const genDrift = findUnregisteredSemverInGeneratedDoc(pins, doc);
	if (genDrift.length > 0) {
		process.stderr.write(
			"[generate-compat-docs] unregistered semver literals in generated doc:\n",
		);
		for (const f of genDrift) {
			process.stderr.write(`  ${f}\n`);
		}
		process.exit(1);
	}

	// Check 2: no registered pin value appears in any other doc
	const treeDrift = findRegisteredPinsInOtherDocs(pins, REPO_ROOT);
	if (treeDrift.length > 0) {
		process.stderr.write(
			"[generate-compat-docs] registered pin values found in other docs:\n",
		);
		for (const f of treeDrift) {
			process.stderr.write(`  ${f}\n`);
		}
		process.exit(1);
	}

	process.stdout.write("COMPAT_DOCS_OK\n");
}

if (import.meta.main) main();
