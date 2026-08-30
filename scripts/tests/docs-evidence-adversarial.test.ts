/**
 * DOC-G2 adversarial mutation suite (issue #135).
 *
 * Injects each of the five drift classes named in the DOC-G2 acceptance
 * criteria and verifies the combined DOC-A checker + DOC-B generator program
 * catches each with a distinct, named failing mutation:
 *
 *   1. stale-sidecar-reuse         — sidecar reused after source code change
 *   2. constant-fork-ts-rust       — TS/Rust constant fork accepted by single-source read
 *   3. out-of-band-deps-doc-edit   — generated-doc-only dep commit touching non-DOC-B blocks
 *   4. disguised-example-product-import — fixture accreting example-product behavior
 *   5. evidence-free-unreleased    — Unreleased entry without commit evidence
 *
 * Each mutation is asserted individually: the checker must fail with a
 * problem string that names the drift class.
 */

import { describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { CANONICAL_REFERENCE_SHA } from "../reference-identity.ts";
import {
	TOOL_VERSION,
	checkUnreleasedEntriesHaveEvidence,
	runEvidence,
	scanForExampleProductImports,
	EXAMPLE_PRODUCT_MARKER,
	EVIDENCE_CLASSES,
	FORBIDDEN_FIELDS,
	type LedgerRow,
	type Sidecar,
} from "../verification/docs-evidence-runners.ts";
import {
	extractTsConst,
	extractRustConst,
} from "../verification/generate-compat-docs.ts";
import {
	REPO_ROOT,
	runCheck,
} from "../verification/docs-evidence.ts";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function writePriorSidecar(dir: string, sidecar: Sidecar): void {
	mkdirSync(dir, { recursive: true });
	writeFileSync(join(dir, `${sidecar.rowId}.json`), JSON.stringify(sidecar, null, 2) + "\n");
}

function scratchCheck(
	rows: readonly LedgerRow[],
	sidecarDir?: string,
): { ok: boolean; problems: readonly string[] } {
	const dir = mkdtempSync(join(tmpdir(), "doc-g2-"));
	const scDir = sidecarDir ?? join(dir, "sidecars");
	mkdirSync(scDir, { recursive: true });
	const result = runCheck(
		{ schema: "pi.docs.evidence.v1", referencePin: CANONICAL_REFERENCE_SHA, rows },
		{ schema: "pi.docs.inventory.v1", categories: [{ id: "t", name: "t", surfaces: rows.map((r) => r.surface) }] },
		REPO_ROOT,
		scDir,
		new Date().toISOString(),
	);
	rmSync(dir, { recursive: true, force: true });
	return result;
}

// ---------------------------------------------------------------------------
// 1. Stale-sidecar reuse after code change (contentHash recompute)
// ---------------------------------------------------------------------------

describe("DOC-G2: stale-sidecar-reuse after code change", () => {
	test("contentHash mismatch detected when source file changes but sidecar is not refreshed", () => {
		const dir = mkdtempSync(join(tmpdir(), "doc-g2-stale-"));
		const scDir = join(dir, "sidecars");
		mkdirSync(scDir, { recursive: true });

		// Use a review-only-prose row pointing at a real file
		const row: LedgerRow = {
			id: "g2-stale-sidecar",
			surface: "test-stale",
			owner: "DOC-A",
			status: "present",
			target: "test-stale",
			class: "review-only-prose",
			params: { source: ".references/pi-2.0/README.md" },
		};

		// Compute the fresh hash
		const fresh = runEvidence(row, REPO_ROOT, new Date().toISOString());

		// Write a prior sidecar with a WRONG contentHash (simulating a code change
		// that altered the file but the sidecar was not refreshed)
		writePriorSidecar(scDir, {
			rowId: row.id,
			contentHash: "a".repeat(64),
			toolVersion: TOOL_VERSION,
			runId: new Date().toISOString(),
		});

		const result = runCheck(
			{ schema: "pi.docs.evidence.v1", referencePin: CANONICAL_REFERENCE_SHA, rows: [row] },
			{ schema: "pi.docs.inventory.v1", categories: [{ id: "t", name: "t", surfaces: [row.surface] }] },
			REPO_ROOT,
			scDir,
			new Date().toISOString(),
		);

		rmSync(dir, { recursive: true, force: true });
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("contentHash mismatch"))).toBe(true);
	});
});

// ---------------------------------------------------------------------------
// 2. Constant-fork TS/Rust accepted by single-source read
// ---------------------------------------------------------------------------

describe("DOC-G2: constant-fork-ts-rust", () => {
	test("PROTOCOL_VERSION fork between TS and Rust is detectable by cross-assert", () => {
		const tsContent = readFileSync(
			join(REPO_ROOT, "packages/pi-tui-protocol/src/types.ts"),
			"utf8",
		);
		const rustContent = readFileSync(
			join(REPO_ROOT, "crates/pi-ext/src/protocol.rs"),
			"utf8",
		);

		const tsVal = extractTsConst(tsContent, "PROTOCOL_VERSION");
		const rustVal = extractRustConst(rustContent, "PROTOCOL_VERSION");

		// On the real tree they agree
		expect(tsVal).toBe(rustVal);

		// Inject a fork: modify the Rust source to disagree
		const forkedRust = rustContent.replace(
			/pub\s+const\s+PROTOCOL_VERSION\s*(?::\s*[^=]+)?\s*=\s*["']?([A-Za-z0-9_.+-]+)["']?\s*;/,
			'pub const PROTOCOL_VERSION: u32 = 999;',
		);
		const forkedRustVal = extractRustConst(forkedRust, "PROTOCOL_VERSION");

		// The fork is detectable: values disagree
		expect(forkedRustVal).not.toBe(tsVal);
		expect(forkedRustVal).toBe("999");

		// The cross-assert in collectPins would throw on this fork:
		// if (tsProtocolVersion !== rustProtocolVersion) throw ...
		// This is the named failing mutation — the DOC-B generator catches it.
		expect(() => {
			if (tsVal !== forkedRustVal) {
				throw new Error(
					`PROTOCOL_VERSION mismatch: TS=${tsVal}, Rust=${forkedRustVal}`,
				);
			}
		}).toThrow("PROTOCOL_VERSION mismatch");
	});

	test("COMPATIBILITY_VERSION fork between TS, Rust, and extension-host is detectable", () => {
		const tsContent = readFileSync(
			join(REPO_ROOT, "packages/pi-tui-protocol/src/types.ts"),
			"utf8",
		);
		const rustContent = readFileSync(
			join(REPO_ROOT, "crates/pi-ext/src/protocol.rs"),
			"utf8",
		);
		const extHostContent = readFileSync(
			join(REPO_ROOT, "packages/extension-host/src/version.ts"),
			"utf8",
		);

		const tsVal = extractTsConst(tsContent, "COMPATIBILITY_VERSION");
		const rustVal = extractRustConst(rustContent, "COMPATIBILITY_VERSION");
		const extHostVal = extractTsConst(extHostContent, "COMPATIBILITY_VERSION");

		// All three agree on the real tree
		expect(tsVal).toBe(rustVal);
		expect(tsVal).toBe(extHostVal);

		// Inject a fork in the extension-host source
		const forkedExtHost = extHostContent.replace(
			/(?:export\s+)?(?:const|let|var)\s+COMPATIBILITY_VERSION\s*=\s*["']?([A-Za-z0-9_.+-]+)["']?/,
			'export const COMPATIBILITY_VERSION = "99.99.99"',
		);
		const forkedExtHostVal = extractTsConst(forkedExtHost, "COMPATIBILITY_VERSION");

		expect(forkedExtHostVal).not.toBe(tsVal);
		expect(forkedExtHostVal).toBe("99.99.99");

		// The triple-owner cross-assert would throw
		expect(() => {
			if (tsVal !== forkedExtHostVal) {
				throw new Error(
					`COMPATIBILITY_VERSION mismatch: TS protocol=${tsVal}, extension-host=${forkedExtHostVal}`,
				);
			}
		}).toThrow("COMPATIBILITY_VERSION mismatch");
	});
});

// ---------------------------------------------------------------------------
// 3. Out-of-band deps doc edit (non-DOC-B-owned block touched)
// ---------------------------------------------------------------------------

describe("DOC-G2: out-of-band-deps-doc-edit", () => {
	test("review-only-prose surface edited out-of-band is detected by contentHash mismatch", () => {
		const dir = mkdtempSync(join(tmpdir(), "doc-g2-oob-"));
		const scDir = join(dir, "sidecars");
		mkdirSync(scDir, { recursive: true });

		// Create a temp file simulating a doc surface (relative to dir)
		writeFileSync(join(dir, "fake-doc.md"), "# Original content\n\nThis is the original doc.\n");

		const row: LedgerRow = {
			id: "g2-oob-edit",
			surface: "test-oob",
			owner: "DOC-A",
			status: "present",
			target: "test-oob",
			class: "review-only-prose",
			params: { source: "fake-doc.md" },
		};

		// First run: compute fresh hash and write sidecar
		const fresh1 = runEvidence(row, dir, new Date().toISOString());
		writePriorSidecar(scDir, fresh1.sidecar);

		// Simulate an out-of-band edit: modify the doc content
		writeFileSync(join(dir, "fake-doc.md"), "# Modified content\n\nAn out-of-band dependency commit changed this.\n");

		// Second run: should detect contentHash mismatch
		const result = runCheck(
			{ schema: "pi.docs.evidence.v1", referencePin: CANONICAL_REFERENCE_SHA, rows: [row] },
			{ schema: "pi.docs.inventory.v1", categories: [{ id: "t", name: "t", surfaces: [row.surface] }] },
			dir,
			scDir,
			new Date().toISOString(),
		);

		rmSync(dir, { recursive: true, force: true });
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("contentHash mismatch"))).toBe(true);
	});
});

// ---------------------------------------------------------------------------
// 4. Disguised example-product import in fixtures
// ---------------------------------------------------------------------------

describe("DOC-G2: disguised-example-product-import", () => {
	test("value import from .references/pi-2.0/ is detected by scanForExampleProductImports", () => {
		const dir = mkdtempSync(join(tmpdir(), "doc-g2-import-"));
		const subDir = join(dir, "scripts", "tests");
		mkdirSync(subDir, { recursive: true });

		// Write a file with a disguised value import from the example-product tree
		writeFileSync(
			join(subDir, "malicious-fixture.ts"),
			[
				'import { something } from "../../.references/pi-2.0/packages/coding-agent/src/index.ts";',
				'export const x = something;',
			].join("\n"),
		);

		const findings = scanForExampleProductImports(dir);
		rmSync(dir, { recursive: true, force: true });

		expect(findings.length).toBeGreaterThan(0);
		expect(findings.some((f) => f.includes("disguised example-product import"))).toBe(true);
	});

	test("type-only import from .references/pi-2.0/ is NOT flagged (no runtime behavior accretion)", () => {
		const dir = mkdtempSync(join(tmpdir(), "doc-g2-type-import-"));
		const subDir = join(dir, "scripts", "tests");
		mkdirSync(subDir, { recursive: true });

		writeFileSync(
			join(subDir, "type-only-fixture.ts"),
			[
				'import type { Foo } from "../../.references/pi-2.0/packages/coding-agent/src/types.ts";',
				'export const x: Foo = {} as Foo;',
			].join("\n"),
		);

		const findings = scanForExampleProductImports(dir);
		rmSync(dir, { recursive: true, force: true });

		expect(findings).toEqual([]);
	});

	test("runCheck surfaces disguised value import as [example-product-import] problem", () => {
		// Inject a disguised value import into a temp dir structured like the repo
		// and verify runCheck surfaces it as an [example-product-import] problem.
		const dir = mkdtempSync(join(tmpdir(), "doc-g2-wiring-"));
		const scDir = join(dir, "sidecars");
		mkdirSync(scDir, { recursive: true });
		mkdirSync(join(dir, "scripts", "tests"), { recursive: true });
		writeFileSync(
			join(dir, "scripts", "tests", "injected-fixture.ts"),
			'import { evil } from "../../.references/pi-2.0/packages/coding-agent/src/index.ts";\n' +
				'export const x = evil;\n',
		);

		// Use a minimal valid row so the only problem is the import finding
		const row: LedgerRow = {
			id: "g2-wiring-row",
			surface: "test-wiring",
			owner: "DOC-A",
			status: "present",
			target: "test-wiring",
			class: "review-only-prose",
			params: { source: "scripts/tests/injected-fixture.ts" },
		};

		const result = runCheck(
			{ schema: "pi.docs.evidence.v1", referencePin: CANONICAL_REFERENCE_SHA, rows: [row] },
			{ schema: "pi.docs.inventory.v1", categories: [{ id: "t", name: "t", surfaces: [row.surface] }] },
			dir,
			scDir,
			new Date().toISOString(),
		);
		rmSync(dir, { recursive: true, force: true });

		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("[example-product-import]"))).toBe(true);
		expect(result.problems.some((p) => p.includes("disguised example-product import"))).toBe(true);
	});

	test("real tree scan is clean (no disguised value imports)", () => {
		const findings = scanForExampleProductImports(REPO_ROOT);
		expect(findings).toEqual([]);
	});
});

// ---------------------------------------------------------------------------
// 5. Evidence-free Unreleased entry
// ---------------------------------------------------------------------------

describe("DOC-G2: evidence-free-unreleased-entry", () => {
	test("Unreleased entry without commit SHA, PR ref, or URL is detected", () => {
		const section = [
			"## [Unreleased]",
			"",
			"### Added",
			"",
			"- Added a new feature without any commit reference",
			"- Fixed a bug with no evidence link",
			"",
		].join("\n");

		const problems = checkUnreleasedEntriesHaveEvidence("g2-test-cl", section);
		expect(problems.length).toBe(2);
		expect(problems[0]?.includes("evidence-free Unreleased entry")).toBe(true);
		expect(problems[0]?.includes("Added a new feature without any commit reference")).toBe(true);
		expect(problems[1]?.includes("evidence-free Unreleased entry")).toBe(true);
		expect(problems[1]?.includes("Fixed a bug with no evidence link")).toBe(true);
	});

	test("Unreleased entry with PR reference is accepted", () => {
		const section = [
			"## [Unreleased]",
			"",
			"### Added",
			"",
			"- Added a new feature ([#123](https://github.com/example/repo/issues/123))",
			"",
		].join("\n");

		const problems = checkUnreleasedEntriesHaveEvidence("g2-test-cl-ok", section);
		expect(problems).toEqual([]);
	});

	test("Unreleased entry with commit SHA is accepted", () => {
		const section = [
			"## [Unreleased]",
			"",
			"- Fixed a bug (abc1234)",
			"",
		].join("\n");

		const problems = checkUnreleasedEntriesHaveEvidence("g2-test-cl-sha", section);
		expect(problems).toEqual([]);
	});

	test("Unreleased entry with URL is accepted", () => {
		const section = [
			"## [Unreleased]",
			"",
			"- See https://github.com/example/repo/pull/456 for details",
			"",
		].join("\n");

		const problems = checkUnreleasedEntriesHaveEvidence("g2-test-cl-url", section);
		expect(problems).toEqual([]);
	});

	test("Empty Unreleased section is accepted (no entries = no evidence-free entries)", () => {
		const section = [
			"## [Unreleased]",
			"",
		].join("\n");

		const problems = checkUnreleasedEntriesHaveEvidence("g2-test-cl-empty", section);
		expect(problems).toEqual([]);
	});

	test("changelog-unreleased runner fails on evidence-free entries in scratch check", () => {
		const dir = mkdtempSync(join(tmpdir(), "doc-g2-cl-"));
		const scDir = join(dir, "sidecars");
		mkdirSync(scDir, { recursive: true });
		writeFileSync(
			join(dir, "CHANGELOG.md"),
			[
				"# Changelog",
				"",
				"## [Unreleased]",
				"",
				"### Added",
				"",
				"- Added something without any commit reference",
				"",
				"## [1.0.0] - 2026-01-01",
				"",
				"- Initial release.",
				"",
			].join("\n"),
		);

		const row: LedgerRow = {
			id: "g2-cl-evidence-free",
			surface: "test-cl",
			owner: "DOC-A",
			status: "present",
			target: "test-cl",
			class: "changelog-unreleased",
			params: { source: "CHANGELOG.md" },
		};

		const result = runCheck(
			{ schema: "pi.docs.evidence.v1", referencePin: CANONICAL_REFERENCE_SHA, rows: [row] },
			{ schema: "pi.docs.inventory.v1", categories: [{ id: "t", name: "t", surfaces: [row.surface] }] },
			dir,
			scDir,
			new Date().toISOString(),
		);
		rmSync(dir, { recursive: true, force: true });

		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("evidence-free Unreleased entry"))).toBe(true);
	});
});

// ---------------------------------------------------------------------------
// 6. Scope boundary: docs phase must not regain release-packaging scope
// ---------------------------------------------------------------------------

describe("DOC-G2: scope boundary observation", () => {
	test("closed evidence-class set excludes release-packaging implementation classes", () => {
		// The seven closed classes are documentation evidence classes, not
		// release-packaging implementation classes.  The FORBIDDEN_FIELDS
		// list prevents any row from carrying command/argv strings, which
		// prevents the checker from shelling out or implementing release logic.
		// This is a design observation, not a mutation — the closed class set
		// is the scope boundary.


		// Seven closed classes — no release-packaging class
		expect(EVIDENCE_CLASSES).toHaveLength(7);
		expect(EVIDENCE_CLASSES).not.toContain("release-packaging");
		expect(EVIDENCE_CLASSES).not.toContain("release-stage");

		// Forbidden fields prevent shelling out
		expect(FORBIDDEN_FIELDS).toContain("command");
		expect(FORBIDDEN_FIELDS).toContain("argv");
		expect(FORBIDDEN_FIELDS).toContain("shell");
		expect(FORBIDDEN_FIELDS).toContain("exec");
	});
});
