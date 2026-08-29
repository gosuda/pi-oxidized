import { describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { spawnSync } from "node:child_process";

import {
	CANONICAL_REFERENCE_SHA,
	STALE_REFERENCE_SHA,
} from "../verification/alignment.ts";
import {
	DEFAULT_REPROOF_INTERVAL_MS,
	EVIDENCE_CLASSES,
	FORBIDDEN_FIELDS,
	RUN_MANIFEST_SCHEMA,
	TOOL_VERSION,
	checkStaleness,
	isEvidenceClass,
	isEvidenceStatus,
	runEvidence,
	sha256,
	type LedgerRow,
	type RunManifest,
	type Sidecar,
} from "../verification/docs-evidence-runners.ts";
import {
	DEFAULT_INVENTORY_PATH,
	DEFAULT_LEDGER_PATH,
	EXPECTED_LEDGER_ROW_COUNT,
	REPO_ROOT,
	RUN_MANIFEST_FILENAME,
	SENTINEL_OK,
	canonicalJson,
	inventorySurfaceCount,
	loadInventory,
	loadLedger,
	runCheck,
	validateLedger,
	type CheckResult,
} from "../verification/docs-evidence.ts";

const LEDGER = loadLedger(REPO_ROOT, DEFAULT_LEDGER_PATH);
const INVENTORY = loadInventory(REPO_ROOT, DEFAULT_INVENTORY_PATH);

/** A minimal valid row for each evidence class. */
function sampleRow(evidenceClass: string, id: string): LedgerRow {
	const base = {
		id,
		surface: `test-${id}`,
		owner: "DOC-A",
		status: "present" as const,
		target: `test-${id}`,
		class: evidenceClass as LedgerRow["class"],
	};
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
): CheckResult & { sidecarDir: string } {
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
		rows.length,
	);
	// We always own the scratch dir; a caller-supplied sidecar dir survives
	// so the caller can inspect artifacts in it (e.g. the run manifest).
	rmSync(dir, { recursive: true, force: true });
	return { ...result, sidecarDir: scDir };
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

	test("CLI entrypoint prints sentinel + runId + rows + manifest and emits the manifest", () => {
		const dir = mkdtempSync(join(tmpdir(), "docs-ev-cli-"));
		try {
			const scDir = join(dir, "sidecars");
			const proc = spawnSync(
				"bun",
				[
					"run",
					"scripts/verification/docs-evidence.ts",
					"--sidecar-dir",
					scDir,
				],
				{ cwd: REPO_ROOT, encoding: "utf8", timeout: 30000 },
			);
			expect(proc.status).toBe(0);

			const manifestPath = join(scDir, RUN_MANIFEST_FILENAME);
			expect(existsSync(manifestPath)).toBe(true);
			const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as RunManifest;
			expect(manifest.schema).toBe(RUN_MANIFEST_SCHEMA);
			expect(manifest.referencePin).toBe(CANONICAL_REFERENCE_SHA);
			expect(manifest.ledgerHash).toBe(sha256(canonicalJson(LEDGER)));
			expect(manifest.rowCount).toBe(LEDGER.rows.length);
			expect(manifest.presentCount).toBe(LEDGER.rows.length);
			const entryIds = manifest.entries.map((e) => e.rowId);
			expect([...entryIds].sort()).toEqual(entryIds); // sorted by rowId
			expect(new Set(entryIds)).toEqual(new Set(LEDGER.rows.map((r) => r.id)));
			for (const entry of manifest.entries) {
				expect(entry.status).toBe("present");
				expect(entry.contentHash).toMatch(/^[0-9a-f]{64}$/);
			}

			const out = proc.stdout.trim();
			expect(out.startsWith(`${SENTINEL_OK} `)).toBe(true);
			expect(out).toContain(`runId=${manifest.runId}`);
			expect(out).toContain(`rows=${LEDGER.rows.length}`);
			expect(out).toContain(`manifest=${manifestPath}`);
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	test("CLI removes a stale manifest when input loading fails", () => {
		const dir = mkdtempSync(join(tmpdir(), "docs-ev-load-fail-"));
		try {
			const ledgerPath = join(dir, "broken-ledger.json");
			const scDir = join(dir, "sidecars");
			mkdirSync(scDir, { recursive: true });
			writeFileSync(ledgerPath, "{");
			const manifestPath = join(scDir, RUN_MANIFEST_FILENAME);
			writeFileSync(manifestPath, '{"schema":"pi.docs.evidence.run.v1"}\n');

			const proc = spawnSync(
				"bun",
				[
					"run",
					"scripts/verification/docs-evidence.ts",
					"--ledger",
					relative(REPO_ROOT, ledgerPath),
					"--sidecar-dir",
					scDir,
				],
				{ cwd: REPO_ROOT, encoding: "utf8", timeout: 30000 },
			);

			expect(proc.status).toBe(1);
			expect(proc.stderr).toContain("failed to load inputs");
			expect(existsSync(manifestPath)).toBe(false);
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	test("CLI rejects a coordinated ledger and inventory omission", () => {
		const dir = mkdtempSync(join(tmpdir(), "docs-ev-row-contract-"));
		try {
			const omittedSurface = LEDGER.rows.at(-1)?.surface;
			expect(omittedSurface).toBeTruthy();
			const ledgerPath = join(dir, "ledger.json");
			const inventoryPath = join(dir, "inventory.json");
			const scDir = join(dir, "sidecars");
			writeFileSync(
				ledgerPath,
				JSON.stringify({ ...LEDGER, rows: LEDGER.rows.slice(0, -1) }, null, 2),
			);
			writeFileSync(
				inventoryPath,
				JSON.stringify(
					{
						...INVENTORY,
						categories: INVENTORY.categories.map((category) => ({
							...category,
							surfaces: category.surfaces.filter((surface) => surface !== omittedSurface),
						})),
					},
					null,
					2,
				),
			);

			const proc = spawnSync(
				"bun",
				[
					"run",
					"scripts/verification/docs-evidence.ts",
					"--ledger",
					relative(REPO_ROOT, ledgerPath),
					"--inventory",
					relative(REPO_ROOT, inventoryPath),
					"--sidecar-dir",
					scDir,
				],
				{ cwd: REPO_ROOT, encoding: "utf8", timeout: 30000 },
			);

			expect(proc.status).toBe(1);
			expect(proc.stderr).toContain(
				`ledger has ${EXPECTED_LEDGER_ROW_COUNT - 1} rows, contract requires ${EXPECTED_LEDGER_ROW_COUNT}`,
			);
			expect(existsSync(join(scDir, RUN_MANIFEST_FILENAME))).toBe(false);
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});
});

describe("docs-evidence: ledger structure", () => {
	test("ledger row count equals the fixed contract and inventory surface count", () => {
		const inventoryCount = inventorySurfaceCount(INVENTORY);
		expect(LEDGER.rows.length).toBe(EXPECTED_LEDGER_ROW_COUNT);
		expect(LEDGER.rows.length).toBe(inventoryCount);
	});

	test("every row carries an owner and a closed class", () => {
		for (const row of LEDGER.rows) {
			expect(row.owner).toBeTruthy();
			expect(isEvidenceClass(row.class)).toBe(true);
		}
	});

	test("every row carries a known status and a nonempty target equal to its surface", () => {
		for (const row of LEDGER.rows) {
			expect(isEvidenceStatus(row.status)).toBe(true);
			expect(row.status).toBe("present");
			expect(row.target.length).toBeGreaterThan(0);
			expect(row.target).toBe(row.surface);
		}
	});

	test("canonicalJson is key-order independent (stable ledger hashing)", () => {
		const a = { schema: "s", referencePin: "r", rows: [{ id: "x", surface: "y" }] };
		const b = { rows: [{ surface: "y", id: "x" }], referencePin: "r", schema: "s" };
		expect(canonicalJson(a)).toBe(canonicalJson(b));
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
			status: "present",
			target: "test",
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
			status: "present",
			target: "test",
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
			status: "present",
			target: "test",
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

	test("missing status fails", () => {
		const { status: _status, ...row } = sampleRow("review-only-prose", "mut-status-missing");
		const result = runScratchCheck([row as LedgerRow]);
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("unknown or missing status"))).toBe(true);
	});

	test("unknown status fails", () => {
		const row = { ...sampleRow("review-only-prose", "mut-status-bogus"), status: "proven" };
		const result = runScratchCheck([row as LedgerRow]);
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("unknown or missing status: proven"))).toBe(true);
	});

	for (const pendingStatus of ["pending-port", "pending-evidence"] as const) {
		test(`${pendingStatus} blocks a successful run manifest`, () => {
			const outer = mkdtempSync(join(tmpdir(), `docs-ev-${pendingStatus}-`));
			const scDir = join(outer, "sidecars");
			const row = {
				...sampleRow("review-only-prose", `mut-${pendingStatus}`),
				status: pendingStatus,
			};
			const result = runScratchCheck([row], CANONICAL_REFERENCE_SHA, scDir);
			try {
				expect(result.ok).toBe(false);
				expect(result.problems.some((p) => p.includes(`status ${pendingStatus} is not final`))).toBe(true);
				expect(result.manifestPath).toBeNull();
				expect(existsSync(join(scDir, RUN_MANIFEST_FILENAME))).toBe(false);
			} finally {
				rmSync(outer, { recursive: true, force: true });
			}
		});
	}

	test("missing target fails", () => {
		const { target: _target, ...row } = sampleRow("review-only-prose", "mut-target-missing");
		const result = runScratchCheck([row as LedgerRow]);
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("missing target"))).toBe(true);
	});

	test("empty target fails", () => {
		const row = { ...sampleRow("review-only-prose", "mut-target-empty"), target: "" };
		const result = runScratchCheck([row as LedgerRow]);
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("missing target"))).toBe(true);
	});

	test("target that differs from its surface fails", () => {
		const row = {
			...sampleRow("review-only-prose", "mut-target-mismatch"),
			target: "different-surface",
		};
		const result = runScratchCheck([row]);
		expect(result.ok).toBe(false);
		expect(result.problems.some((p) => p.includes("does not match surface"))).toBe(true);
	});

	test("clean run writes a run manifest with sorted present entries", () => {
		const outer = mkdtempSync(join(tmpdir(), "docs-ev-manifest-ok-"));
		const scDir = join(outer, "sidecars");
		const rows = [
			sampleRow("review-only-prose", "zz-manifest-prose"),
			sampleRow("changelog-unreleased", "aa-manifest-changelog"),
		];
		const result = runScratchCheck(rows, CANONICAL_REFERENCE_SHA, scDir);
		const manifestPath = join(scDir, RUN_MANIFEST_FILENAME);
		try {
			expect(result.ok).toBe(true);
			expect(result.manifestPath).toBe(manifestPath);
			const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as RunManifest;
			expect(manifest.schema).toBe(RUN_MANIFEST_SCHEMA);
			expect(manifest.referencePin).toBe(CANONICAL_REFERENCE_SHA);
			expect(manifest.ledgerHash).toBe(
				sha256(canonicalJson({ schema: "pi.docs.evidence.v1", referencePin: CANONICAL_REFERENCE_SHA, rows })),
			);
			expect(manifest.rowCount).toBe(2);
			expect(manifest.presentCount).toBe(2);
			expect(manifest.entries.map((e) => e.rowId)).toEqual([
				"aa-manifest-changelog",
				"zz-manifest-prose",
			]);
			for (const entry of manifest.entries) {
				expect(entry.status).toBe("present");
				expect(entry.contentHash).toMatch(/^[0-9a-f]{64}$/);
			}
		} finally {
			rmSync(outer, { recursive: true, force: true });
		}
	});

	test("failed run removes a stale manifest and writes none", () => {
		const outer = mkdtempSync(join(tmpdir(), "docs-ev-manifest-fail-"));
		const scDir = join(outer, "sidecars");
		mkdirSync(scDir, { recursive: true });
		const manifestPath = join(scDir, RUN_MANIFEST_FILENAME);
		writeFileSync(manifestPath, '{"schema":"pi.docs.evidence.run.v1"}\n');
		const row = { ...sampleRow("review-only-prose", "mut-manifest-fail"), status: "bogus" };
		const result = runScratchCheck([row as LedgerRow], CANONICAL_REFERENCE_SHA, scDir);
		try {
			expect(result.ok).toBe(false);
			expect(result.manifestPath).toBeNull();
			expect(existsSync(manifestPath)).toBe(false);
		} finally {
			rmSync(outer, { recursive: true, force: true });
		}
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
	const row: LedgerRow = { id: "stale-unit", surface: "s", owner: "DOC-A", status: "present", target: "s", class: "review-only-prose", params: { source: "x" } };

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
