import { describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

import {
	CANONICAL_REFERENCE_SHA,
	STALE_REFERENCE_SHA,
} from "../verification/alignment.ts";
import {
	DEFAULT_REPROOF_INTERVAL_MS,
	EVIDENCE_CLASSES,
	FORBIDDEN_FIELDS,
	TOOL_VERSION,
	checkStaleness,
	isEvidenceClass,
	runEvidence,
	type LedgerRow,
	type Sidecar,
} from "../verification/docs-evidence-runners.ts";
import {
	DEFAULT_INVENTORY_PATH,
	DEFAULT_LEDGER_PATH,
	REPO_ROOT,
	SENTINEL_OK,
	inventorySurfaceCount,
	loadInventory,
	loadLedger,
	runCheck,
	validateLedger,
} from "../verification/docs-evidence.ts";

const LEDGER = loadLedger(REPO_ROOT, DEFAULT_LEDGER_PATH);
const INVENTORY = loadInventory(REPO_ROOT, DEFAULT_INVENTORY_PATH);

/** A minimal valid row for each evidence class. */
function sampleRow(evidenceClass: string, id: string): LedgerRow {
	const base = { id, surface: `test-${id}`, owner: "DOC-A", class: evidenceClass as LedgerRow["class"] };
	switch (evidenceClass) {
		case "version-pin":
			return { ...base, params: { label: "PROTOCOL_VERSION", expected: "1", source: "packages/pi-tui-protocol/src/types.ts" } };
		case "generated-block":
			return { ...base, params: { generator: "scripts/release/stage.ts", artifact: "release.json" } };
		case "fenced-compile":
			return { ...base, params: { topic: "docs/evidence.md", fenceMarker: "fence-test-marker" } };
		case "transcript-claim":
			return { ...base, params: { source: "scripts/release/args.ts", claim: "--target" } };
		case "matrix-count":
			return { ...base, params: { source: "scripts/verification/compat-matrix.json", expectedCount: 35, countMethod: "json-array", countKey: "rows" } };
		case "review-only-prose":
			return { ...base, params: { source: ".references/pi/README.md" } };
		case "changelog-unreleased":
			return { ...base, params: { source: ".references/pi/packages/coding-agent/CHANGELOG.md" } };
		default:
			return { ...base, params: {} };
	}
}

/** Write a scratch ledger + inventory to a temp dir and run the checker. */
function runScratchCheck(
	rows: readonly LedgerRow[],
	referencePin: string = CANONICAL_REFERENCE_SHA,
	sidecarDir?: string,
): { ok: boolean; problems: readonly string[] } {
	const dir = mkdtempSync(join(tmpdir(), "docs-ev-"));
	const ledgerPath = join(dir, "ledger.json");
	const inventoryPath = join(dir, "inventory.json");
	const scDir = sidecarDir ?? join(dir, "sidecars");
	mkdirSync(scDir, { recursive: true });

	writeFileSync(
		ledgerPath,
		JSON.stringify({ schema: "pi.docs.evidence.v1", referencePin, rows }, null, 2),
	);
	writeFileSync(
		inventoryPath,
		JSON.stringify({
			schema: "pi.docs.inventory.v1",
			categories: [{ id: "test", name: "test", surfaces: rows.map((r) => r.surface) }],
		}),
	);
	const result = runCheck(
		{ schema: "pi.docs.evidence.v1", referencePin, rows },
		{ schema: "pi.docs.inventory.v1", categories: [{ id: "test", name: "test", surfaces: rows.map((r) => r.surface) }] },
		REPO_ROOT,
		scDir,
		new Date().toISOString(),
	);
	rmSync(dir, { recursive: true, force: true });
	return result;
}

/** Write a prior sidecar to a dir so the checker can detect staleness. */
function writePriorSidecar(dir: string, sidecar: Sidecar): void {
	mkdirSync(dir, { recursive: true });
	writeFileSync(join(dir, `${sidecar.rowId}.json`), JSON.stringify(sidecar, null, 2) + "\n");
}

describe("docs-evidence: green on current tree", () => {
	test("checker exits 0 on the current tree", () => {
		const dir = mkdtempSync(join(tmpdir(), "docs-ev-green-"));
		const scDir = join(dir, "sidecars");
		mkdirSync(scDir, { recursive: true });
		const result = runCheck(LEDGER, INVENTORY, REPO_ROOT, scDir, new Date().toISOString());
		rmSync(dir, { recursive: true, force: true });
		expect(result.ok).toBe(true);
		expect(result.problems).toEqual([]);
	});

	test("CLI entrypoint produces the OK sentinel", () => {
		const proc = spawnSync(
			"bun",
			["run", "scripts/verification/docs-evidence.ts", "--sidecar-dir", join(tmpdir(), "docs-ev-cli")],
			{ cwd: REPO_ROOT, encoding: "utf8", timeout: 30000 },
		);
		expect(proc.status).toBe(0);
		expect(proc.stdout.trim()).toBe(SENTINEL_OK);
	});
});

describe("docs-evidence: ledger structure", () => {
	test("ledger row count equals inventory surface count", () => {
		const expected = inventorySurfaceCount(INVENTORY);
		expect(LEDGER.rows.length).toBe(expected);
	});

	test("every row carries an owner and a closed class", () => {
		for (const row of LEDGER.rows) {
			expect(row.owner).toBeTruthy();
			expect(isEvidenceClass(row.class)).toBe(true);
		}
	});

	test("exactly one reference-pin literal is recorded", () => {
		expect(LEDGER.referencePin).toBe(CANONICAL_REFERENCE_SHA);
		const raw = JSON.stringify(LEDGER);
		const occurrences = raw.split(CANONICAL_REFERENCE_SHA).length - 1;
		expect(occurrences).toBe(1);
	});

	test("no row carries a forbidden command/argv field", () => {
		for (const row of LEDGER.rows) {
			const rowRecord = row as unknown as Record<string, unknown>;
			for (const field of FORBIDDEN_FIELDS) {
				expect(Object.hasOwn(rowRecord, field)).toBe(false);
				expect(Object.hasOwn(row.params, field)).toBe(false);
			}
		}
	});

	test("seven closed evidence classes are declared", () => {
		expect(EVIDENCE_CLASSES).toHaveLength(7);
		const expected = [
			"changelog-unreleased",
			"fenced-compile",
			"generated-block",
			"matrix-count",
			"review-only-prose",
			"transcript-claim",
			"version-pin",
		] as const;
		expect([...EVIDENCE_CLASSES].sort() as readonly string[]).toEqual(
			[...expected].sort() as readonly string[],
		);
	});
});

// ---------------------------------------------------------------------------
// Mutation suite — one test per class, asserted individually
// ---------------------------------------------------------------------------

describe("docs-evidence: mutation suite (per class)", () => {
	test("version-pin: missing runner param fails", () => {
		const row: LedgerRow = {
			id: "mut-vp-missing",
			surface: "test",
			owner: "DOC-A",
			class: "version-pin",
			params: { label: "PROTOCOL_VERSION", source: "packages/pi-tui-protocol/src/types.ts" },
		};
		const result = runScratchCheck([row]);
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("missing required param: expected"))).toBe(true);
	});

	test("unknown class name fails", () => {
		const row = {
			id: "mut-unknown-class",
			surface: "test",
			owner: "DOC-A",
			class: "nonexistent-class",
			params: {},
		};
		const result = runScratchCheck([row as unknown as LedgerRow]);
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("unknown") && p.includes("class"))).toBe(true);
	});

	test("tampered contentHash fails", () => {
		const dir = mkdtempSync(join(tmpdir(), "docs-ev-tamper-"));
		const scDir = join(dir, "sidecars");
		mkdirSync(scDir, { recursive: true });
		const row = sampleRow("review-only-prose", "mut-tamper");
		// Write a prior sidecar with a wrong contentHash
		writePriorSidecar(scDir, {
			rowId: row.id,
			contentHash: "0".repeat(64),
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

	test("stale toolVersion fails", () => {
		const dir = mkdtempSync(join(tmpdir(), "docs-ev-stale-tv-"));
		const scDir = join(dir, "sidecars");
		mkdirSync(scDir, { recursive: true });
		const row = sampleRow("review-only-prose", "mut-stale-tv");
		// First run to produce a fresh sidecar
		runCheck(
			{ schema: "pi.docs.evidence.v1", referencePin: CANONICAL_REFERENCE_SHA, rows: [row] },
			{ schema: "pi.docs.inventory.v1", categories: [{ id: "t", name: "t", surfaces: [row.surface] }] },
			REPO_ROOT,
			scDir,
			new Date().toISOString(),
		);
		// Overwrite the sidecar with a stale toolVersion
		const freshResult = runEvidence(row, REPO_ROOT, new Date().toISOString());
		writePriorSidecar(scDir, {
			...freshResult.sidecar,
			toolVersion: "pi.docs.evidence.v0",
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
		expect(result.problems.some((p) => p.includes("toolVersion mismatch"))).toBe(true);
	});

	test("runId older than required re-proof fails", () => {
		const dir = mkdtempSync(join(tmpdir(), "docs-ev-old-run-"));
		const scDir = join(dir, "sidecars");
		mkdirSync(scDir, { recursive: true });
		const row = sampleRow("review-only-prose", "mut-old-runid");
		const freshResult = runEvidence(row, REPO_ROOT, new Date().toISOString());
		// Write a prior sidecar with a runId 30 days ago
		const oldDate = new Date(Date.now() - 30 * 24 * 60 * 60 * 1000).toISOString();
		writePriorSidecar(scDir, {
			...freshResult.sidecar,
			runId: oldDate,
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
		expect(result.problems.some((p) => p.includes("days old"))).toBe(true);
	});

	test("arbitrary command field present fails", () => {
		const row = {
			id: "mut-command-field",
			surface: "test",
			owner: "DOC-A",
			class: "review-only-prose",
			params: { source: ".references/pi/README.md" },
			command: "bun run something",
		};
		const result = runScratchCheck([row as unknown as LedgerRow]);
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("forbidden field"))).toBe(true);
	});

	test("review-only surface edited without matching non-stale evidence fails", () => {
		const dir = mkdtempSync(join(tmpdir(), "docs-ev-edited-"));
		const scDir = join(dir, "sidecars");
		mkdirSync(scDir, { recursive: true });
		const row = sampleRow("review-only-prose", "mut-edited-prose");
		// Write a prior sidecar whose contentHash doesn't match the current content
		writePriorSidecar(scDir, {
			rowId: row.id,
			contentHash: "f".repeat(64),
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
// Reference-pin literal tests
// ---------------------------------------------------------------------------

describe("docs-evidence: reference-pin literal", () => {
	test("stale hash injected into scratch ledger fails the checker", () => {
		const row = sampleRow("review-only-prose", "stale-pin-test");
		const result = runScratchCheck([row], STALE_REFERENCE_SHA);
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("stale hash") || p.includes(STALE_REFERENCE_SHA))).toBe(true);
	});

	test("validateLedger rejects a non-canonical referencePin", () => {
		const problems = validateLedger({
			schema: "pi.docs.evidence.v1",
			referencePin: "abcdef0000000000000000000000000000000000",
			rows: [],
		});
		expect(problems.some((p) => p.message.includes("referencePin"))).toBe(true);
	});

	test("validateLedger accepts the canonical referencePin", () => {
		const problems = validateLedger({
			schema: "pi.docs.evidence.v1",
			referencePin: CANONICAL_REFERENCE_SHA,
			rows: [],
		});
		expect(problems.filter((p) => p.message.includes("referencePin"))).toEqual([]);
	});
});

// ---------------------------------------------------------------------------
// Staleness unit tests
// ---------------------------------------------------------------------------

describe("docs-evidence: staleness logic", () => {
	const now = new Date().toISOString();
	const row: LedgerRow = { id: "stale-unit", surface: "s", owner: "DOC-A", class: "review-only-prose", params: { source: "x" } };

	test("matching sidecar is not stale", () => {
		const sc: Sidecar = { rowId: row.id, contentHash: "abc", toolVersion: TOOL_VERSION, runId: now };
		expect(checkStaleness(sc, sc, DEFAULT_REPROOF_INTERVAL_MS).stale).toBe(false);
	});

	test("contentHash mismatch is stale", () => {
		const prior: Sidecar = { rowId: row.id, contentHash: "aaa", toolVersion: TOOL_VERSION, runId: now };
		const fresh: Sidecar = { rowId: row.id, contentHash: "bbb", toolVersion: TOOL_VERSION, runId: now };
		expect(checkStaleness(prior, fresh, DEFAULT_REPROOF_INTERVAL_MS).stale).toBe(true);
	});

	test("toolVersion mismatch is stale", () => {
		const prior: Sidecar = { rowId: row.id, contentHash: "abc", toolVersion: "old", runId: now };
		const fresh: Sidecar = { rowId: row.id, contentHash: "abc", toolVersion: TOOL_VERSION, runId: now };
		expect(checkStaleness(prior, fresh, DEFAULT_REPROOF_INTERVAL_MS).stale).toBe(true);
	});

	test("runId older than reproof interval is stale", () => {
		const old = new Date(Date.now() - 30 * 24 * 60 * 60 * 1000).toISOString();
		const prior: Sidecar = { rowId: row.id, contentHash: "abc", toolVersion: TOOL_VERSION, runId: old };
		const fresh: Sidecar = { rowId: row.id, contentHash: "abc", toolVersion: TOOL_VERSION, runId: now };
		expect(checkStaleness(prior, fresh, DEFAULT_REPROOF_INTERVAL_MS).stale).toBe(true);
	});

	test("runId within reproof interval is not stale", () => {
		const recent = new Date(Date.now() - 3 * 24 * 60 * 60 * 1000).toISOString();
		const prior: Sidecar = { rowId: row.id, contentHash: "abc", toolVersion: TOOL_VERSION, runId: recent };
		const fresh: Sidecar = { rowId: row.id, contentHash: "abc", toolVersion: TOOL_VERSION, runId: now };
		expect(checkStaleness(prior, fresh, DEFAULT_REPROOF_INTERVAL_MS).stale).toBe(false);
	});
});
