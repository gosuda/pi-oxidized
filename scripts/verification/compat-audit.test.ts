import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { readFileSync, writeFileSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import {
	REPO_ROOT,
	parseCompatSource,
	verifySourceEvidence,
	verifyDownstreamImporters,
	verifyExtensionHostRouting,
	verifyConfigCorpus,
	verifyNoRustCompatConsumer,
	runCompatAuditWitnesses,
} from "./compat-audit.ts";

const REF_ROOT = join(REPO_ROOT, ".references", "pi", "packages");
const COMPAT_TS = join(REF_ROOT, "ai", "src", "compat.ts");

const temporaryPaths: string[] = [];
function temporaryDirectory(prefix: string): string {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	temporaryPaths.push(dir);
	return dir;
}

function writeNested(path: string, content: string): void {
	mkdirSync(join(path, ".."), { recursive: true });
	writeFileSync(path, content);
}

describe("compat audit witness suite", () => {
	test("real repository passes every witness", () => {
		expect(runCompatAuditWitnesses(REPO_ROOT)).toEqual([]);
	});

	// --- Witness 1: source evidence ---

	test("source parser extracts re-exports, direct exports, and side effects", () => {
		const source = readFileSync(COMPAT_TS, "utf8");
		const inv = parseCompatSource(source);
		expect(inv.reexports.length).toBe(17);
		expect(inv.directExports).toContain("BuiltinProvider");
		expect(inv.directExports).toContain("stream");
		expect(inv.sideEffects).toContain("registerBuiltInApiProviders();");
		expect(inv.sideEffects).toContain("const compatModels = builtinModels();");
		expect(inv.hasEnvImport).toBe(true);
	});

	test("missing re-export fails source evidence witness", () => {
		const source = readFileSync(COMPAT_TS, "utf8");
		const mutated = source.replace(
			'export * from "./env-api-keys.ts";',
			'// export * from "./env-api-keys.ts";',
		);
		const violations = verifySourceEvidence(mutated);
		expect(violations.some((v) => v.includes("env-api-keys.ts"))).toBe(true);
	});

	test("missing side effect fails source evidence witness", () => {
		const source = readFileSync(COMPAT_TS, "utf8");
		const mutated = source.replaceAll(
			"registerBuiltInApiProviders();",
			"// registerBuiltInApiProviders();",
		);
		const violations = verifySourceEvidence(mutated);
		expect(violations.some((v) => v.includes("registerBuiltInApiProviders"))).toBe(true);
	});

	test("missing direct export fails source evidence witness", () => {
		const source = readFileSync(COMPAT_TS, "utf8");
		const mutated = source.replace(
			"export function stream<",
			"function stream<",
		);
		const violations = verifySourceEvidence(mutated);
		expect(violations.some((v) => v.includes('"stream"'))).toBe(true);
	});

	// --- Witness 2: downstream importers ---

	test("downstream importer enumeration finds all TS-side-runtime consumers", () => {
		const violations = verifyDownstreamImporters(REF_ROOT);
		expect(violations).toEqual([]);
	});

	test("downstream importer witness catches unexpected package", () => {
		const dir = temporaryDirectory("compat-audit-pkg-");
		writeNested(
			join(dir, "unexpected-pkg", "src", "mod.ts"),
			'import { stream } from "@earendil-works/pi-ai/compat";',
		);
		for (const pkg of ["ai", "agent", "coding-agent"]) {
			writeNested(join(dir, pkg, "src", "dummy.ts"), "");
		}
		const violations = verifyDownstreamImporters(dir);
		expect(violations.some((v) => v.includes("unexpected package"))).toBe(true);
	});

	// --- Witness 3: extension-host routing ---

	test("extension-host routing witness passes on real repo", () => {
		expect(verifyExtensionHostRouting(REPO_ROOT)).toEqual([]);
	});

	test("removing compat alias from virtual-modules fails routing witness", () => {
		const dir = temporaryDirectory("compat-audit-routing-");
		const extHost = join(dir, "packages", "extension-host", "src");
		writeNested(join(extHost, "virtual-modules.ts"), "const x = 1;\n");
		writeNested(
			join(extHost, "host.ts"),
			'import { validateToolArguments } from "@earendil-works/pi-ai/compat";',
		);
		const violations = verifyExtensionHostRouting(dir);
		expect(violations.some((v) => v.includes("does not route"))).toBe(true);
	});

	test("removing host.ts compat import fails routing witness", () => {
		const dir = temporaryDirectory("compat-audit-host-");
		const extHost = join(dir, "packages", "extension-host", "src");
		writeNested(
			join(extHost, "virtual-modules.ts"),
			'"@earendil-works/pi-ai": _bundledPiAiCompat,\n"@earendil-works/pi-ai/compat": _bundledPiAiCompat,\nconst aiCompat = `${REF_ROOT}/ai/src/compat.ts`;\n',
		);
		writeNested(join(extHost, "host.ts"), "const x = 1;\n");
		const violations = verifyExtensionHostRouting(dir);
		expect(violations.some((v) => v.includes("host.ts does not import"))).toBe(true);
	});

	// --- Witness 4: config corpus ---

	test("config corpus witness passes on real repo", () => {
		expect(verifyConfigCorpus(REPO_ROOT)).toEqual([]);
	});

	test("missing env_keys.rs fails config corpus witness", () => {
		const dir = temporaryDirectory("compat-audit-config-");
		const violations = verifyConfigCorpus(dir);
		expect(violations.some((v) => v.includes("env_keys.rs not readable"))).toBe(true);
	});

	test("missing Model.compat field fails config corpus witness", () => {
		const dir = temporaryDirectory("compat-audit-model-");
		const authDir = join(dir, "crates", "pi-ai", "src", "auth");
		writeNested(
			join(authDir, "env_keys.rs"),
			"pub fn get_env_api_key() {}\npub fn find_env_keys() {}\nconst OPENAI_API_KEY = \"\";\nconst ANTHROPIC_API_KEY = \"\";\nconst GEMINI_API_KEY = \"\";\n",
		);
		const typesDir = join(dir, "crates", "pi-ai", "src");
		writeNested(join(typesDir, "types.rs"), "pub struct Model {}\n");
		const violations = verifyConfigCorpus(dir);
		expect(violations.some((v) => v.includes("Model.compat"))).toBe(true);
		expect(violations.some((v) => v.includes("model.compat"))).toBe(true);
	});

	// --- Witness 5: Rust-surface negative ---

	test("Rust-surface negative witness passes on real repo", () => {
		expect(verifyNoRustCompatConsumer(REPO_ROOT)).toEqual([]);
	});

	test("Rust file with pi_ai::compat reference fails negative witness", () => {
		const dir = temporaryDirectory("compat-audit-rust-");
		const crateDir = join(dir, "crates", "pi-ai", "src");
		writeNested(join(crateDir, "mod.rs"), "use pi_ai::compat::stream;\n");
		const violations = verifyNoRustCompatConsumer(dir);
		expect(violations.some((v) => v.includes("pi_ai::compat"))).toBe(true);
	});

	test("Rust file with mod compat declaration fails negative witness", () => {
		const dir = temporaryDirectory("compat-audit-mod-");
		const crateDir = join(dir, "crates", "pi-ai", "src");
		writeNested(join(crateDir, "mod.rs"), "pub mod compat;\n");
		const violations = verifyNoRustCompatConsumer(dir);
		expect(violations.some((v) => v.includes("mod compat"))).toBe(true);
	});

	// --- Cleanup ---

	test("cleanup temporary directories", () => {
		for (const path of temporaryPaths) {
			rmSync(path, { recursive: true, force: true });
		}
		expect(temporaryPaths.length).toBeGreaterThan(0);
	});
});
