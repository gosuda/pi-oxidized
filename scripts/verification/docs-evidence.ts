#!/usr/bin/env bun
/**
 * Doc-evidence ledger checker entrypoint (DOC-A, issue #129).
 *
 * Loads the docs-evidence ledger, validates the schema (no command/argv
 * strings, every row carries an owner and a closed class, exactly one
 * reference-pin literal), runs each row through its closed evidence-class
 * runner, checks sidecar staleness (contentHash + toolVersion + runId), and
 * writes fresh sidecar artifacts under target/verification/docs-evidence/.
 *
 * The ledger row count is asserted programmatically against the inventory
 * artifact (scripts/verification/fixtures/docs-inventory.json).
 *
 * Usage:
 *   bun run scripts/verification/docs-evidence.ts
 *   bun run scripts/verification/docs-evidence.ts --ledger <path> --sidecar-dir <dir> --inventory <path>
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

import {
	CANONICAL_REFERENCE_SHA,
	STALE_REFERENCE_SHA,
} from "./alignment.ts";
import {
	DEFAULT_REPROOF_INTERVAL_MS,
	FORBIDDEN_FIELDS,
	TOOL_VERSION,
	type LedgerRow,
	type RunnerResult,
	type Sidecar,
	checkStaleness,
	isEvidenceClass,
	runEvidence,
} from "./docs-evidence-runners.ts";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

export const DEFAULT_LEDGER_PATH = "scripts/verification/docs-evidence.json";
export const DEFAULT_INVENTORY_PATH = "scripts/verification/fixtures/docs-inventory.json";
export const DEFAULT_SIDECAR_DIR = "target/verification/docs-evidence";

export const SENTINEL_OK = "DOCS_EVIDENCE_OK";

// ---------------------------------------------------------------------------
// CLI parsing
// ---------------------------------------------------------------------------

interface CliArgs {
	readonly ledgerPath: string;
	readonly inventoryPath: string;
	readonly sidecarDir: string;
}

function parseArgs(argv: readonly string[]): CliArgs {
	const args: CliArgs = {
		ledgerPath: DEFAULT_LEDGER_PATH,
		inventoryPath: DEFAULT_INVENTORY_PATH,
		sidecarDir: DEFAULT_SIDECAR_DIR,
	};
	for (let i = 0; i < argv.length; i++) {
		const flag = argv[i];
		const next = argv[i + 1];
		if (flag === "--ledger" && next) {
			(args as { ledgerPath: string }).ledgerPath = next;
			i++;
		} else if (flag === "--inventory" && next) {
			(args as { inventoryPath: string }).inventoryPath = next;
			i++;
		} else if (flag === "--sidecar-dir" && next) {
			(args as { sidecarDir: string }).sidecarDir = next;
			i++;
		}
	}
	return args;
}

// ---------------------------------------------------------------------------
// Ledger loading and validation
// ---------------------------------------------------------------------------

export interface Ledger {
	readonly schema: string;
	readonly referencePin: string;
	readonly rows: readonly LedgerRow[];
}

export interface InventoryArtifact {
	readonly schema: string;
	readonly categories: readonly {
		readonly id: string;
		readonly name: string;
		readonly surfaces: readonly string[];
	}[];
}

export function loadLedger(root: string, relPath: string): Ledger {
	const abs = join(root, relPath);
	if (!existsSync(abs)) {
		throw new Error(`ledger not found: ${relPath}`);
	}
	const raw = JSON.parse(readFileSync(abs, "utf8")) as Record<string, unknown>;
	if (raw["schema"] !== "pi.docs.evidence.v1") {
		throw new Error(`ledger schema mismatch: ${String(raw["schema"])}`);
	}
	if (typeof raw["referencePin"] !== "string") {
		throw new Error("ledger missing referencePin");
	}
	const rows = raw["rows"];
	if (!Array.isArray(rows)) {
		throw new Error("ledger rows is not an array");
	}
	return {
		schema: raw["schema"] as string,
		referencePin: raw["referencePin"] as string,
		rows: rows as LedgerRow[],
	};
}

export function loadInventory(root: string, relPath: string): InventoryArtifact {
	const abs = join(root, relPath);
	if (!existsSync(abs)) {
		throw new Error(`inventory not found: ${relPath}`);
	}
	const raw = JSON.parse(readFileSync(abs, "utf8")) as Record<string, unknown>;
	if (raw["schema"] !== "pi.docs.inventory.v1") {
		throw new Error(`inventory schema mismatch: ${String(raw["schema"])}`);
	}
	return raw as unknown as InventoryArtifact;
}

/** Count total surfaces in the inventory artifact. */
export function inventorySurfaceCount(inv: InventoryArtifact): number {
	let count = 0;
	for (const cat of inv.categories) {
		count += cat.surfaces.length;
	}
	return count;
}

// ---------------------------------------------------------------------------
// Schema validation
// ---------------------------------------------------------------------------

export interface ValidationProblem {
	readonly rowId: string;
	readonly message: string;
}

export function validateLedger(ledger: Ledger): readonly ValidationProblem[] {
	const problems: ValidationProblem[] = [];

	// Reference-pin literal check
	if (ledger.referencePin !== CANONICAL_REFERENCE_SHA) {
		problems.push({
			rowId: "(ledger)",
			message: `referencePin is ${ledger.referencePin}, expected ${CANONICAL_REFERENCE_SHA}`,
		});
	}
	if (ledger.referencePin === STALE_REFERENCE_SHA) {
		problems.push({
			rowId: "(ledger)",
			message: `referencePin is the stale hash ${STALE_REFERENCE_SHA}`,
		});
	}

	const seenIds = new Set<string>();
	for (const row of ledger.rows) {
		// Unique id
		if (seenIds.has(row.id)) {
			problems.push({ rowId: row.id, message: "duplicate row id" });
		}
		seenIds.add(row.id);

		// Owner required
		if (typeof row.owner !== "string" || row.owner.length === 0) {
			problems.push({ rowId: row.id, message: "missing owner" });
		}

		// Closed class required
		if (!isEvidenceClass(row.class)) {
			problems.push({
				rowId: row.id,
				message: `unknown or missing evidence class: ${String(row.class)}`,
			});
		}

		// No forbidden fields (command/argv strings)
		const rowRecord = row as unknown as Record<string, unknown>;
		for (const field of FORBIDDEN_FIELDS) {
			if (Object.hasOwn(rowRecord, field) || Object.hasOwn(row.params, field)) {
				problems.push({
					rowId: row.id,
					message: `forbidden field present: ${field}`,
				});
			}
		}

		// Params must be an object
		if (typeof row.params !== "object" || row.params === null) {
			problems.push({ rowId: row.id, message: "params is not an object" });
		}
	}

	return problems;
}

// ---------------------------------------------------------------------------
// Sidecar I/O
// ---------------------------------------------------------------------------

function sidecarPath(sidecarDir: string, rowId: string): string {
	return join(sidecarDir, `${rowId}.json`);
}

function readPriorSidecar(sidecarDir: string, rowId: string): Sidecar | null {
	const p = sidecarPath(sidecarDir, rowId);
	if (!existsSync(p)) return null;
	try {
		return JSON.parse(readFileSync(p, "utf8")) as Sidecar;
	} catch {
		return null;
	}
}

function writeSidecar(sidecarDir: string, sidecar: Sidecar): void {
	mkdirSync(sidecarDir, { recursive: true });
	writeFileSync(sidecarPath(sidecarDir, sidecar.rowId), JSON.stringify(sidecar, null, 2) + "\n");
}

// ---------------------------------------------------------------------------
// Main check loop
// ---------------------------------------------------------------------------

export interface CheckResult {
	readonly ok: boolean;
	readonly problems: readonly string[];
	readonly sidecars: readonly Sidecar[];
}

/**
 * Run the full doc-evidence check: validate ledger, run each row's evidence
 * class runner, check sidecar staleness, and assert row count against the
 * inventory artifact.
 */
export function runCheck(
	ledger: Ledger,
	inventory: InventoryArtifact,
	root: string,
	sidecarDir: string,
	runId: string,
): CheckResult {
	const problems: string[] = [];
	const sidecars: Sidecar[] = [];

	// 1. Schema validation
	const validationProblems = validateLedger(ledger);
	for (const vp of validationProblems) {
		problems.push(`[validation] ${vp.rowId}: ${vp.message}`);
	}

	// 2. Row count vs inventory
	const expectedCount = inventorySurfaceCount(inventory);
	if (ledger.rows.length !== expectedCount) {
		problems.push(
			`[inventory] ledger has ${ledger.rows.length} rows, inventory has ${expectedCount} surfaces`,
		);
	}

	// 3. Run each row's evidence class runner
	for (const row of ledger.rows) {
		let result: RunnerResult;
		try {
			result = runEvidence(row, root, runId);
		} catch (error) {
			const detail = error instanceof Error ? error.message : String(error);
			problems.push(`[runner] ${row.id}: runner threw: ${detail}`);
			continue;
		}

		if (!result.ok) {
			for (const p of result.problems) {
				problems.push(`[runner] ${p}`);
			}
		}

		// 4. Check staleness against prior sidecar
		const prior = readPriorSidecar(sidecarDir, row.id);
		if (prior !== null) {
			const staleness = checkStaleness(prior, result.sidecar, DEFAULT_REPROOF_INTERVAL_MS);
			for (const reason of staleness.reasons) {
				problems.push(`[stale] ${reason}`);
			}
		}

		sidecars.push(result.sidecar);
	}

	// 5. Write fresh sidecars (even if there were problems, so the next run can compare)
	for (const sc of sidecars) {
		writeSidecar(sidecarDir, sc);
	}

	return { ok: problems.length === 0, problems, sidecars };
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
	const args = parseArgs(process.argv.slice(2));
	const root = REPO_ROOT;
	const sidecarDir = resolve(root, args.sidecarDir);

	let ledger: Ledger;
	let inventory: InventoryArtifact;
	try {
		ledger = loadLedger(root, args.ledgerPath);
		inventory = loadInventory(root, args.inventoryPath);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		console.error(`docs-evidence: failed to load inputs: ${detail}`);
		process.exit(1);
	}

	const runId = new Date().toISOString();
	const result = runCheck(ledger, inventory, root, sidecarDir, runId);

	if (result.ok) {
		process.stdout.write(SENTINEL_OK + "\n");
		return;
	}

	console.error(`docs-evidence: ${result.problems.length} problem(s):`);
	for (const p of result.problems) {
		console.error(`  - ${p}`);
	}
	process.exit(1);
}

if (import.meta.main) main();
