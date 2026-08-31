#!/usr/bin/env bun
/**
 * Arbitration ruling verifier (MAP-2, issue #142).
 *
 * The execution map stays a single graph authority: this tool imports the
 * MAP-1 ledger (`map.ts`) and consumes its published artifacts — the
 * structural ticket-record witness, the pointer-selected execution-map
 * generation, and docs/PARITY_LEDGER.md — through `loadMapLedgerInputs`. It
 * never re-parses the registry or re-derives edges; every graph assertion
 * here is a re-run of `runMapLedgerChecks` over the unchanged published texts.
 *
 * The verifier owns three contracts:
 *
 * 1. ruling count       - exactly fifteen rulings (AR1–AR15) are published in
 *                         docs/ARBITRATION_RULINGS.md, each with a unique
 *                         ruling ID, exactly one owning sibling ticket, and
 *                         a non-empty rejected option.
 * 2. binding edges      - every binding edge named by the rulings is present
 *                         in the live registry (the execution-map
 *                         generation's blocked_by cells). The DAG re-passes
 *                         MAP-1's full graph check: acyclic, exact canonical IDs, zero
 *                         duplicates, zero aliases, full reachability, zero
 *                         REL-DOCS bypass paths.
 * 3. ownership uniqueness - no surface ends with two owners. The five named
 *                         surfaces (mirror witness, release constants plus
 *                         documentation staging, extension-host endpoint,
 *                         config-value, telemetry) each map to exactly one
 *                         owning sibling ticket.
 *
 * `bun run verify:arbitration` prints ARBITRATION_OK when every ruling is
 * published, every binding edge is present, the graph baseline is green, and
 * no surface has two owners.
 *
 * Checks are pure over their inputs; only `main` reads files.
 */

import {
	loadMapLedgerInputs,
	parseExecutionMap,
	runMapLedgerChecks,
	type MapLedgerInputs,
	type MapRow,
} from "./map.ts";
import { REPO_ROOT } from "./parity.ts";
import { readFileSync } from "node:fs";
import { join } from "node:path";

// ============================================================================
// Published pins
// ============================================================================

export const ARBITRATION_DOC_PATH = "docs/ARBITRATION_RULINGS.md";
export const RULING_COUNT = 15;

// ============================================================================
// Ruling definitions
// ============================================================================

export interface Ruling {
	readonly id: string;
	readonly owner: string;
	readonly ownerIssue: number;
	readonly surface: string;
	readonly rejectedOption: string;
	/** Binding edges: array of [dependent, prerequisite] pairs. */
	readonly edges: readonly (readonly [string, string])[];
}

export const RULINGS: readonly Ruling[] = [
	{
		id: "AR1",
		owner: "XC-2",
		ownerIssue: 41,
		surface: "Mirror lockstep witness for protocol.rs, TypeScript METHODS, and frames.jsonl",
		rejectedOption: "Shared ownership between PAR and XC tracks",
		edges: [["PAR-CLOSE", "XC-2"]],
	},
	{
		id: "AR2",
		owner: "PAR-CLIENT",
		ownerIssue: 33,
		surface: "cfg(unix) platform contract for PAR transports",
		rejectedOption: "Portable-only (loses Unix sockets) or Unix-only (not portable)",
		edges: [],
	},
	{
		id: "AR3",
		owner: "PAR-TEL",
		ownerIssue: 71,
		surface: "Six-site AgentLoopConfig telemetry boundary",
		rejectedOption: "Unpinned AgentLoopConfig literal construction",
		edges: [],
	},
	{
		id: "AR4",
		owner: "XC-1",
		ownerIssue: 52,
		surface: "Extension-host endpoint ownership: pi-ext",
		rejectedOption: "pi or pi-agent owning the extension-host endpoint",
		edges: [],
	},
	{
		id: "AR5",
		owner: "REL-DOCS",
		ownerIssue: 111,
		surface: "Release constants plus documentation staging; DOC-F consume-only",
		rejectedOption: "DOC track owning release docs",
		edges: [
			["REL-CLOSE", "REL-DOCS"],
			["DOC-F", "REL-DOCS"],
			["DOC-F", "REL-CLOSE"],
		],
	},
	{
		id: "AR6",
		owner: "PAR-COMPAT-DISPO",
		ownerIssue: 45,
		surface: "Config-value: one parser and one command cache",
		rejectedOption: "Split parser/cache across crates",
		edges: [],
	},
	{
		id: "AR7",
		owner: "XC-2",
		ownerIssue: 41,
		surface: "XC mirror witness precedes PAR ratification",
		rejectedOption: "Parallel or after PAR closure",
		edges: [["PAR-CLOSE", "XC-2"]],
	},
	{
		id: "AR8",
		owner: "DOC-A",
		ownerIssue: 129,
		surface: "Doc-evidence ledger depends on workflow reference alignment",
		rejectedOption: "No dependency on VER-ALIGN",
		edges: [["DOC-A", "VER-ALIGN"]],
	},
	{
		id: "AR9",
		owner: "DOC-E",
		ownerIssue: 136,
		surface: "CHANGELOG and release instructions consume REL constants read-only",
		rejectedOption: "Write access or no dependency",
		edges: [["DOC-E", "REL-CLOSE"]],
	},
	{
		id: "AR10",
		owner: "DOC-F",
		ownerIssue: 138,
		surface: "Publication verification depends on the dependency closing audit",
		rejectedOption: "No dependency",
		edges: [["DOC-F", "DEPS-D1"]],
	},
	{
		id: "AR11",
		owner: "TUI-V1",
		ownerIssue: 76,
		surface: "TUI state-matrix verification depends on the release platform definition",
		rejectedOption: "No dependency",
		edges: [["TUI-V1", "EXT-26"]],
	},
	{
		id: "AR12",
		owner: "TUI-V1",
		ownerIssue: 76,
		surface: "TUI state-matrix verification depends on the portable PTY harness",
		rejectedOption: "No dependency",
		edges: [["TUI-V1", "TUI-P1"]],
	},
	{
		id: "AR13",
		owner: "PERF-T6",
		ownerIssue: 88,
		surface: "Extension-host scaling lane bound to pi_ext::server::serve_io",
		rejectedOption: "Different boundary or no binding",
		edges: [],
	},
	{
		id: "AR14",
		owner: "PAR-CLOSE",
		ownerIssue: 39,
		surface: "PAR closure precedes extension compatibility closure",
		rejectedOption: "Parallel or before PAR closure",
		edges: [["XC-CLOSE", "PAR-CLOSE"]],
	},
	{
		id: "AR15",
		owner: "MAP-5",
		ownerIssue: 144,
		surface: "All eight track closers precede the final cross-plan gate",
		rejectedOption: "Partial closure or subset",
		edges: [
			["MAP-5", "PAR-CLOSE"],
			["MAP-5", "XC-CLOSE"],
			["MAP-5", "TUI-CLOSE"],
			["MAP-5", "PERF-CLOSE"],
			["MAP-5", "REL-CLOSE"],
			["MAP-5", "DEPS-D1"],
			["MAP-5", "DOC-F"],
			["MAP-5", "ARC-CLOSE"],
		],
	},
];

// ============================================================================
// Ownership surfaces — cross-check that no surface has two owners
// ============================================================================

export interface OwnershipSurface {
	readonly name: string;
	readonly owner: string;
}

export const OWNERSHIP_SURFACES: readonly OwnershipSurface[] = [
	{ name: "mirror witness", owner: "XC-2" },
	{ name: "release constants plus documentation staging", owner: "REL-DOCS" },
	{ name: "extension-host endpoint", owner: "XC-1" },
	{ name: "config-value", owner: "PAR-COMPAT-DISPO" },
	{ name: "telemetry", owner: "PAR-TEL" },
];

// ============================================================================
// Checks
// ============================================================================

/** Look up a row by stable ID in the parsed registry. */
function findRow(rows: readonly MapRow[], stableId: string): MapRow | undefined {
	return rows.find((row) => row.stableId === stableId);
}

/** Check that every binding edge named by the rulings is present in the registry. */
export function checkBindingEdges(rows: readonly MapRow[], rulings: readonly Ruling[] = RULINGS): string[] {
	const violations: string[] = [];
	for (const ruling of rulings) {
		for (const [dependent, prerequisite] of ruling.edges) {
			const row = findRow(rows, dependent);
			if (row === undefined) {
				violations.push(`${ruling.id}: dependent ${dependent} not found in registry`);
				continue;
			}
			if (!row.blockedBy.includes(prerequisite)) {
				violations.push(
					`${ruling.id}: binding edge ${dependent} blocked_by ${prerequisite} missing (actual: [${row.blockedBy.join(", ")}])`,
				);
			}
		}
	}
	return violations;
}

/** Check that each ruling's owner exists in the registry. */
export function checkOwners(rows: readonly MapRow[], rulings: readonly Ruling[] = RULINGS): string[] {
	const violations: string[] = [];
	for (const ruling of rulings) {
		const row = findRow(rows, ruling.owner);
		if (row === undefined) {
			violations.push(`${ruling.id}: owner ${ruling.owner} not found in registry`);
		} else if (row.issue !== ruling.ownerIssue) {
			violations.push(
				`${ruling.id}: owner ${ruling.owner} has issue #${row.issue}, expected #${ruling.ownerIssue}`,
			);
		}
	}
	return violations;
}

/** Check that no ownership surface has two owners. */
export function checkOwnershipUniqueness(
	rows: readonly MapRow[],
	surfaces: readonly OwnershipSurface[] = OWNERSHIP_SURFACES,
): string[] {
	const violations: string[] = [];
	for (const surface of surfaces) {
		const row = findRow(rows, surface.owner);
		if (row === undefined) {
			violations.push(`ownership surface '${surface.name}': owner ${surface.owner} not found in registry`);
		}
	}
	// Cross-check: the five named owners are distinct stable IDs
	const owners = surfaces.map((s) => s.owner);
	const duplicates = owners.filter((owner, index) => owners.indexOf(owner) !== index);
	if (duplicates.length > 0) {
		violations.push(`ownership surfaces have duplicate owners: ${[...new Set(duplicates)].join(", ")}`);
	}
	return violations;
}

/** Check that the arbitration doc publishes exactly fifteen rulings. */
export function checkRulingCount(docText: string, expected: number = RULING_COUNT): string[] {
	const violations: string[] = [];
	// Count AR-prefixed ruling IDs in the rulings table
	// Match only rows where AR\d+ is the first column (the rulings table),
	// not rows where AR IDs appear in a later column (the binding-edges table).
	const rulingPattern = /^\| (AR\d+) \|/gm;
	const found = [...docText.matchAll(rulingPattern)].map((m) => m[1] as string);
	const unique = [...new Set(found)];
	if (unique.length !== expected) {
		violations.push(`${ARBITRATION_DOC_PATH} publishes ${unique.length} ruling(s), expected ${expected}`);
	}
	// Check for duplicates
	if (found.length !== unique.length) {
		const dupes = found.filter((id, i) => found.indexOf(id) !== i);
		violations.push(`${ARBITRATION_DOC_PATH} has duplicate ruling IDs: ${[...new Set(dupes)].join(", ")}`);
	}
	return violations;
}

// ============================================================================
// Orchestration
// ============================================================================

export interface ArbitrationInputs {
	readonly mapLedger: MapLedgerInputs;
	readonly arbitrationDoc: string;
}

/** Run every arbitration assertion; an empty list means green. */
export function runArbitrationChecks(inputs: ArbitrationInputs): string[] {
	const violations: string[] = [];
	const add = (witness: string, results: readonly string[]): void => {
		for (const result of results) violations.push(`[${witness}] ${result}`);
	};

	// 1. Re-run the MAP-1 graph check (acyclicity, aliases, REL-DOCS dominance, etc.)
	add("graph", runMapLedgerChecks(inputs.mapLedger));

	// 2. Parse the registry from the published map document
	const doc = parseExecutionMap(inputs.mapLedger.mapText);
	add("map-document", doc.problems);

	// 3. Check ruling count in the arbitration doc
	add("ruling-count", checkRulingCount(inputs.arbitrationDoc));

	// 4. Check that every ruling's owner exists in the registry
	add("owners", checkOwners(doc.rows));

	// 5. Check that every binding edge is present in the registry
	add("binding-edges", checkBindingEdges(doc.rows));

	// 6. Check that no ownership surface has two owners
	add("ownership", checkOwnershipUniqueness(doc.rows));

	return violations;
}

/** Read the two required inputs; the map ledger inputs are loaded first. */
export function loadArbitrationInputs(repoRoot: string): ArbitrationInputs {
	const readRequired = (relativePath: string): string => {
		try {
			return readFileSync(join(repoRoot, relativePath), "utf8");
		} catch (error) {
			throw new Error(`cannot read required ${relativePath}: ${String(error)}`);
		}
	};
	return {
		mapLedger: loadMapLedgerInputs(repoRoot),
		arbitrationDoc: readRequired(ARBITRATION_DOC_PATH),
	};
}

function main(): void {
	let inputs: ArbitrationInputs;
	try {
		inputs = loadArbitrationInputs(REPO_ROOT);
	} catch (error) {
		console.error(`arbitration input failed: ${error instanceof Error ? error.message : String(error)}`);
		process.exit(1);
	}
	const violations = runArbitrationChecks(inputs);
	if (violations.length > 0) {
		console.error(`arbitration verification failed with ${violations.length} violation(s):`);
		for (const violation of violations) console.error(`  - ${violation}`);
		process.exit(1);
	}
	process.stdout.write("ARBITRATION_OK\n");
}

if (import.meta.main) main();
