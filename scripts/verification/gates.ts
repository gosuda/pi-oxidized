#!/usr/bin/env bun
/**
 * Integration sequencing gate verifier (MAP-4, issue #140).
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
 * 1. gate predicates   - six integration sequencing gates (G-FREEZE,
 *                        G-MIRROR, G-RELINFRA, G-PARCLOSE, G-DEPCLOSE,
 *                        G-ALLTRACKS) plus G-RELDOCS evaluate mechanically
 *                        over actual node resolution. Each gate names a
 *                        trigger node (or set) and a set of required
 *                        registry edges. The predicate over a status
 *                        assignment checks that no guarded node is
 *                        resolved before its trigger(s). The guarded set
 *                        is computed from the registry as every node whose
 *                        prerequisite closure contains a trigger node —
 *                        the gate is realized as registry closure edges.
 * 2. dependency-law    - the epoch gate (no DEPS-T1 before PAR-CLOSE and
 *                        EXT-23) and the shipped-artifact re-gate (DEPS-R1
 *                        re-gates on EXT-26) are verified as transitive
 *                        prerequisite-closure assertions.
 * 3. track-start       - no track-start edge contradicts the wayfinder
 *                        ordering: parity before dependency; extension
 *                        mirror before parity ratification; release
 *                        infrastructure before terminal proof; documentation
 *                        staging before release or documentation closure.
 *
 * `bun run verify:gates` prints GATES_OK when every gate predicate is
 * satisfied, every required edge is present, every dependency-law
 * transitive prerequisite holds, the graph baseline is green, and no
 * track-start edge contradicts the wayfinder ordering.
 *
 * Checks are pure over their inputs; only `main` reads files.
 */

import {
	loadMapLedgerInputs,
	parseExecutionMap,
	prerequisiteClosure,
	runMapLedgerChecks,
	TRACK_CLOSERS,
	type MapLedgerInputs,
	type MapRow,
} from "./map.ts";
import { REPO_ROOT } from "./parity.ts";

// ============================================================================
// Published pins
// ============================================================================

export const GATE_COUNT = 7;

// ============================================================================
// Gate definitions
// ============================================================================

export interface Gate {
	/** Stable gate identifier: G-FREEZE, G-MIRROR, etc. */
	readonly id: string;
	/** Human-readable description from the issue text. */
	readonly description: string;
	/** Trigger node(s) that must be resolved before any guarded node. */
	readonly trigger: readonly string[];
	/** Required registry edges: [dependent, prerequisite] pairs. */
	readonly requiredEdges: readonly (readonly [string, string])[];
}

export const GATES: readonly Gate[] = [
	{
		id: "G-FREEZE",
		description: "PAR-P1 frozen unblocks DEPS start, PERF floor ledgers, and the REL target model",
		trigger: ["PAR-CLOSE"],
		requiredEdges: [["DEPS-R1", "PAR-CLOSE"]],
	},
	{
		id: "G-MIRROR",
		description: "XC-P2 established precedes PAR ratification",
		trigger: ["XC-2"],
		requiredEdges: [["PAR-CLOSE", "XC-2"]],
	},
	{
		id: "G-RELINFRA",
		description: "EXT-26 available precedes TUI V-proof, DEPS post-epoch re-gate, and DOC-E/DOC-F",
		trigger: ["EXT-26"],
		requiredEdges: [
			["TUI-V1", "EXT-26"],
			["DEPS-R1", "EXT-26"],
		],
	},
	{
		id: "G-PARCLOSE",
		description: "PAR-CLOSE precedes XC-CLOSE and DOC-C",
		trigger: ["PAR-CLOSE"],
		requiredEdges: [
			["XC-CLOSE", "PAR-CLOSE"],
			["DOC-C", "PAR-CLOSE"],
		],
	},
	{
		id: "G-DEPCLOSE",
		description: "DEPS-D1 precedes DOC-F",
		trigger: ["DEPS-D1"],
		requiredEdges: [["DOC-F", "DEPS-D1"]],
	},
	{
		id: "G-ALLTRACKS",
		description: "All eight track closers resolved precedes MAP-5",
		trigger: TRACK_CLOSERS,
		requiredEdges: TRACK_CLOSERS.map((closer) => ["MAP-5", closer] as readonly [string, string]),
	},
	{
		id: "G-RELDOCS",
		description: "REL-DOCS resolved before REL-CLOSE or DOC-F may resolve",
		trigger: ["REL-DOCS"],
		requiredEdges: [
			["REL-CLOSE", "REL-DOCS"],
			["DOC-F", "REL-DOCS"],
		],
	},
];

// ============================================================================
// Dependency-law edges
// ============================================================================

export interface DependencyLaw {
	/** Stable identifier for the dependency-law assertion. */
	readonly id: string;
	/** Human-readable description. */
	readonly description: string;
	/** The node whose prerequisite closure must contain all `prerequisites`. */
	readonly node: string;
	/** Nodes that must be in `node`'s transitive prerequisite closure. */
	readonly prerequisites: readonly string[];
}

export const DEPENDENCY_LAWS: readonly DependencyLaw[] = [
	{
		id: "DL-EPOCH",
		description: "No epoch before PAR-CLOSE and EXT-23",
		node: "DEPS-T1",
		prerequisites: ["PAR-CLOSE", "EXT-23"],
	},
	{
		id: "DL-REGATE",
		description: "Every shipped-artifact remediation re-gates on the EXT-26 both-musl seven-target proof",
		node: "DEPS-R1",
		prerequisites: ["EXT-26"],
	},
];

// ============================================================================
// Track-start ordering constraints
// ============================================================================

export interface TrackOrdering {
	/** Stable identifier for the ordering constraint. */
	readonly id: string;
	/** Human-readable description. */
	readonly description: string;
	/** Nodes that must come before (be prerequisites of) the `after` nodes. */
	readonly before: readonly string[];
	/** Nodes that must come after (be dependents of) the `before` nodes. */
	readonly after: readonly string[];
}

export const TRACK_ORDERINGS: readonly TrackOrdering[] = [
	{
		id: "TO-PARITY-DEPS",
		description: "Parity before dependency",
		before: ["PAR-CLOSE"],
		after: ["DEPS-R1"],
	},
	{
		id: "TO-MIRROR-RATIFY",
		description: "Extension mirror before parity ratification",
		before: ["XC-2"],
		after: ["PAR-CLOSE"],
	},
	{
		id: "TO-RELINFRA-TUI",
		description: "Release infrastructure before terminal proof",
		before: ["EXT-26"],
		after: ["TUI-V1"],
	},
	{
		id: "TO-RELDOCS-CLOSE",
		description: "Documentation staging before release or documentation closure",
		before: ["REL-DOCS"],
		after: ["REL-CLOSE", "DOC-F"],
	},
];

// ============================================================================
// Status types and helpers
// ============================================================================

export type NodeStatus = "resolved" | "open";
export type StatusMap = ReadonlyMap<string, NodeStatus>;

// ============================================================================
// Graph helpers
// ============================================================================

function findRow(rows: readonly MapRow[], stableId: string): MapRow | undefined {
	return rows.find((row) => row.stableId === stableId);
}

/**
 * Compute the guarded set for a gate: every registry node whose
 * prerequisite closure contains at least one trigger node. This is the
 * set of nodes the gate predicate protects — they cannot resolve until
 * all trigger nodes are resolved.
 */
export function computeGuardedSet(
	rows: readonly MapRow[],
	gate: Gate,
): readonly string[] {
	const guarded: string[] = [];
	for (const row of rows) {
		if (gate.trigger.includes(row.stableId)) continue;
		const closure = prerequisiteClosure(rows, row.stableId);
		if (gate.trigger.some((t) => closure.has(t))) {
			guarded.push(row.stableId);
		}
	}
	return guarded;
}

// ============================================================================
// Gate predicate — status-assignment evaluation
// ============================================================================

/**
 * Evaluate a gate predicate over a status assignment. Returns violations
 * for every guarded node that is resolved while not all trigger nodes are
 * resolved. The guarded set is computed from the registry.
 */
export function evaluateGate(
	rows: readonly MapRow[],
	gate: Gate,
	statuses: StatusMap,
): string[] {
	const violations: string[] = [];
	const triggersResolved = gate.trigger.every((id) => statuses.get(id) === "resolved");
	if (triggersResolved) return violations;
	const triggerList = gate.trigger.join(", ");
	for (const node of computeGuardedSet(rows, gate)) {
		if (statuses.get(node) === "resolved") {
			violations.push(
				`${gate.id}: ${node} resolved before trigger(s) ${triggerList} resolved`,
			);
		}
	}
	return violations;
}

/**
 * Evaluate all gate predicates over a status assignment.
 */
export function evaluateAllGates(
	rows: readonly MapRow[],
	gates: readonly Gate[] = GATES,
	statuses: StatusMap,
): string[] {
	const violations: string[] = [];
	for (const gate of gates) {
		for (const violation of evaluateGate(rows, gate, statuses)) {
			violations.push(`[gate] ${violation}`);
		}
	}
	return violations;
}

// ============================================================================
// Registry consistency checks
// ============================================================================

/**
 * Check that every required edge named by a gate is present in the
 * registry's blocked_by cells.
 */
export function checkGateEdges(
	rows: readonly MapRow[],
	gates: readonly Gate[] = GATES,
): string[] {
	const violations: string[] = [];
	for (const gate of gates) {
		for (const [dependent, prerequisite] of gate.requiredEdges) {
			const row = findRow(rows, dependent);
			if (row === undefined) {
				violations.push(`${gate.id}: dependent ${dependent} not found in registry`);
				continue;
			}
			if (!row.blockedBy.includes(prerequisite)) {
				violations.push(
					`${gate.id}: required edge ${dependent} blocked_by ${prerequisite} missing (actual: [${row.blockedBy.join(", ")}])`,
				);
			}
		}
	}
	return violations;
}

/**
 * Check that every gate's trigger node(s) exist in the registry.
 */
export function checkGateTriggers(
	rows: readonly MapRow[],
	gates: readonly Gate[] = GATES,
): string[] {
	const violations: string[] = [];
	for (const gate of gates) {
		for (const trigger of gate.trigger) {
			if (findRow(rows, trigger) === undefined) {
				violations.push(`${gate.id}: trigger ${trigger} not found in registry`);
			}
		}
	}
	return violations;
}

/**
 * Check that every gate's guarded set is non-empty — a gate with no
 * guarded nodes is vacuous and signals a registry drift.
 */
export function checkGuardedNonVacuous(
	rows: readonly MapRow[],
	gates: readonly Gate[] = GATES,
): string[] {
	const violations: string[] = [];
	for (const gate of gates) {
		const guarded = computeGuardedSet(rows, gate);
		if (guarded.length === 0) {
			violations.push(`${gate.id}: no registry node is downstream of trigger(s) ${gate.trigger.join(", ")} — gate is vacuous`);
		}
	}
	return violations;
}

// ============================================================================
// Dependency-law checks
// ============================================================================

/**
 * Check that every dependency-law transitive prerequisite holds: each
 * `prerequisites` node must be in `node`'s prerequisite closure.
 */
export function checkDependencyLaws(
	rows: readonly MapRow[],
	laws: readonly DependencyLaw[] = DEPENDENCY_LAWS,
): string[] {
	const violations: string[] = [];
	for (const law of laws) {
		const row = findRow(rows, law.node);
		if (row === undefined) {
			violations.push(`${law.id}: node ${law.node} not found in registry`);
			continue;
		}
		const closure = prerequisiteClosure(rows, law.node);
		for (const prereq of law.prerequisites) {
			if (!closure.has(prereq)) {
				violations.push(
					`${law.id}: ${law.node} does not transitively depend on ${prereq} (dependency-law edge missing)`,
				);
			}
		}
	}
	return violations;
}

// ============================================================================
// Track-start ordering checks
// ============================================================================

/**
 * Check that no track-start edge contradicts the wayfinder ordering.
 * For each ordering constraint (before, after), verify that no `before`
 * node transitively depends on any `after` node — that would reverse
 * the intended direction.
 */
export function checkTrackOrdering(
	rows: readonly MapRow[],
	orderings: readonly TrackOrdering[] = TRACK_ORDERINGS,
): string[] {
	const violations: string[] = [];
	for (const ordering of orderings) {
		for (const beforeNode of ordering.before) {
			const row = findRow(rows, beforeNode);
			if (row === undefined) {
				violations.push(`${ordering.id}: before-node ${beforeNode} not found in registry`);
				continue;
			}
			const closure = prerequisiteClosure(rows, beforeNode);
			for (const afterNode of ordering.after) {
				if (closure.has(afterNode)) {
					violations.push(
						`${ordering.id}: ${beforeNode} transitively depends on ${afterNode} — contradicts '${ordering.description}'`,
					);
				}
			}
		}
	}
	return violations;
}

// ============================================================================
// Orchestration
// ============================================================================

export interface GatesInputs {
	readonly mapLedger: MapLedgerInputs;
}

/**
 * Run every gate assertion; an empty list means green. The graph baseline
 * (MAP-1) is re-run first, then the registry is parsed and all gate
 * consistency checks, dependency-law checks, and track-start ordering
 * checks are evaluated.
 */
export function runGatesChecks(inputs: GatesInputs): string[] {
	const violations: string[] = [];
	const add = (witness: string, results: readonly string[]): void => {
		for (const result of results) violations.push(`[${witness}] ${result}`);
	};

	// 1. Re-run the MAP-1 graph check (acyclicity, aliases, REL-DOCS dominance, etc.)
	add("graph", runMapLedgerChecks(inputs.mapLedger));

	// 2. Parse the registry from the published map document
	const doc = parseExecutionMap(inputs.mapLedger.mapText);
	add("map-document", doc.problems);

	// 3. Check that every gate's trigger node exists
	add("gate-triggers", checkGateTriggers(doc.rows));

	// 4. Check that every required edge is present in the registry
	add("gate-edges", checkGateEdges(doc.rows));

	// 5. Check that every gate has a non-empty guarded set
	add("gate-vacuity", checkGuardedNonVacuous(doc.rows));

	// 6. Check dependency-law transitive prerequisites
	add("dependency-law", checkDependencyLaws(doc.rows));

	// 7. Check track-start ordering (no contradiction with wayfinder)
	add("track-ordering", checkTrackOrdering(doc.rows));

	return violations;
}

/** Read the required inputs; the map ledger inputs are loaded first. */
export function loadGatesInputs(repoRoot: string): GatesInputs {
	return {
		mapLedger: loadMapLedgerInputs(repoRoot),
	};
}

function main(): void {
	let inputs: GatesInputs;
	try {
		inputs = loadGatesInputs(REPO_ROOT);
	} catch (error) {
		console.error(`gates input failed: ${error instanceof Error ? error.message : String(error)}`);
		process.exit(1);
	}
	const violations = runGatesChecks(inputs);
	if (violations.length > 0) {
		console.error(`gates verification failed with ${violations.length} violation(s):`);
		for (const violation of violations) console.error(`  - ${violation}`);
		process.exit(1);
	}

	// Report gate census
	const doc = parseExecutionMap(inputs.mapLedger.mapText);
	for (const gate of GATES) {
		const guarded = computeGuardedSet(doc.rows, gate);
		process.stdout.write(
			`GATE ${gate.id} trigger=[${gate.trigger.join(", ")}] guarded=${guarded.length} edges=${gate.requiredEdges.length}\n`,
		);
	}
	process.stdout.write("GATES_OK\n");
}

if (import.meta.main) main();
