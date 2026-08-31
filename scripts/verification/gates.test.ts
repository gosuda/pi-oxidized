import { describe, expect, test } from "bun:test";
import {
	loadMapLedgerInputs,
	parseExecutionMap,
	prerequisiteClosure,
	runMapLedgerChecks,
	type MapLedgerInputs,
	type MapRow,
} from "./map.ts";
import { REPO_ROOT } from "./parity.ts";
import {
	DEPENDENCY_LAWS,
	GATES,
	GATE_COUNT,
	TRACK_ORDERINGS,
	checkDependencyLaws,
	checkGateEdges,
	checkGateTriggers,
	checkGuardedNonVacuous,
	checkTrackOrdering,
	computeGuardedSet,
	evaluateAllGates,
	evaluateGate,
	loadGatesInputs,
	runGatesChecks,
	type Gate,
	type StatusMap,
} from "./gates.ts";

const INPUTS = loadMapLedgerInputs(REPO_ROOT);
const DOC = parseExecutionMap(INPUTS.mapText);
const ROWS = DOC.rows;

/** Remove a prerequisite from a row's blocked_by list (mutation helper). */
function removeEdge(rows: readonly MapRow[], dependent: string, prerequisite: string): MapRow[] {
	return rows.map((row) =>
		row.stableId === dependent
			? { ...row, blockedBy: row.blockedBy.filter((b) => b !== prerequisite) }
			: row,
	);
}

/** Build a status map where only the named nodes are resolved. */
function statusesFor(resolved: readonly string[]): StatusMap {
	return new Map(ROWS.map((row) => [row.stableId, resolved.includes(row.stableId) ? "resolved" : "open" as const]));
}

/** Build a status map where all nodes are resolved except the named ones. */
function statusesExcept(open: readonly string[]): StatusMap {
	return new Map(ROWS.map((row) => [row.stableId, open.includes(row.stableId) ? "open" as const : "resolved" as const]));
}

// ============================================================================
// Gate definitions
// ============================================================================

describe("gate definitions (MAP-4)", () => {
	test("exactly seven gates are defined", () => {
		expect(GATES).toHaveLength(GATE_COUNT);
	});

	test("every gate has a unique id", () => {
		const ids = GATES.map((g) => g.id);
		expect(new Set(ids).size).toBe(ids.length);
	});

	test("every gate has at least one trigger and one required edge", () => {
		for (const gate of GATES) {
			expect(gate.trigger.length).toBeGreaterThan(0);
			expect(gate.requiredEdges.length).toBeGreaterThan(0);
		}
	});

	test("gate ids are the six named gates plus G-RELDOCS", () => {
		const expected = ["G-FREEZE", "G-MIRROR", "G-RELINFRA", "G-PARCLOSE", "G-DEPCLOSE", "G-ALLTRACKS", "G-RELDOCS"];
		expect(GATES.map((g) => g.id).sort()).toEqual(expected.sort());
	});
});

// ============================================================================
// Gate triggers
// ============================================================================

describe("gate triggers (MAP-4)", () => {
	test("every gate trigger exists in the registry", () => {
		expect(checkGateTriggers(ROWS)).toEqual([]);
	});

	test("a gate with a nonexistent trigger fails", () => {
		const badGates: Gate[] = [{ ...GATES[0]!, trigger: ["NONEXISTENT"] }];
		expect(checkGateTriggers(ROWS, badGates)).toEqual([
			"G-FREEZE: trigger NONEXISTENT not found in registry",
		]);
	});
});

// ============================================================================
// Required edges
// ============================================================================

describe("required edges (MAP-4)", () => {
	test("every required edge is present in the registry", () => {
		expect(checkGateEdges(ROWS)).toEqual([]);
	});

	test("removing the DEPS-R1<-PAR-CLOSE edge fails G-FREEZE", () => {
		const mutated = removeEdge(ROWS, "DEPS-R1", "PAR-CLOSE");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-FREEZE: required edge DEPS-R1 blocked_by PAR-CLOSE missing (actual: [EXT-23, EXT-26])",
		);
	});

	test("removing the PAR-CLOSE<-XC-2 edge fails G-MIRROR", () => {
		const mutated = removeEdge(ROWS, "PAR-CLOSE", "XC-2");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-MIRROR: required edge PAR-CLOSE blocked_by XC-2 missing (actual: [PAR-FOLD, PAR-CLIENT, PAR-SERVER, PAR-COMPAT-AUDIT, PAR-COMPAT-DISPO, PAR-PTY-GRILL])",
		);
	});

	test("removing the TUI-V1<-EXT-26 edge fails G-RELINFRA", () => {
		const mutated = removeEdge(ROWS, "TUI-V1", "EXT-26");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-RELINFRA: required edge TUI-V1 blocked_by EXT-26 missing (actual: [TUI-P1, TUI-T1, TUI-T5])",
		);
	});

	test("removing the DEPS-R1<-EXT-26 edge fails G-RELINFRA", () => {
		const mutated = removeEdge(ROWS, "DEPS-R1", "EXT-26");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-RELINFRA: required edge DEPS-R1 blocked_by EXT-26 missing (actual: [EXT-23, PAR-CLOSE])",
		);
	});

	test("removing the XC-CLOSE<-PAR-CLOSE edge fails G-PARCLOSE", () => {
		const mutated = removeEdge(ROWS, "XC-CLOSE", "PAR-CLOSE");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-PARCLOSE: required edge XC-CLOSE blocked_by PAR-CLOSE missing (actual: [XC-2, XC-3, XC-4, XC-5, XC-6, XC-7, XC-8, XC-9])",
		);
	});

	test("removing the DOC-C<-PAR-CLOSE edge fails G-PARCLOSE", () => {
		const mutated = removeEdge(ROWS, "DOC-C", "PAR-CLOSE");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-PARCLOSE: required edge DOC-C blocked_by PAR-CLOSE missing (actual: [DOC-A, DOC-B, DOC-G2, XC-CLOSE, TUI-CLOSE, EXT-25])",
		);
	});

	test("removing the DOC-F<-DEPS-D1 edge fails G-DEPCLOSE", () => {
		const mutated = removeEdge(ROWS, "DOC-F", "DEPS-D1");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-DEPCLOSE: required edge DOC-F blocked_by DEPS-D1 missing (actual: [PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, REL-DOCS, DOC-D, DOC-E])",
		);
	});

	test("removing a MAP-5<-closer edge fails G-ALLTRACKS", () => {
		const mutated = removeEdge(ROWS, "MAP-5", "PAR-CLOSE");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-ALLTRACKS: required edge MAP-5 blocked_by PAR-CLOSE missing (actual: [MAP-4, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E, ARC-CLOSE])",
		);
	});

	test("removing the MAP-5<-ARC-CLOSE edge fails G-ALLTRACKS", () => {
		const mutated = removeEdge(ROWS, "MAP-5", "ARC-CLOSE");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-ALLTRACKS: required edge MAP-5 blocked_by ARC-CLOSE missing (actual: [MAP-4, PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E])",
		);
	});

	test("removing the REL-CLOSE<-REL-DOCS edge fails G-RELDOCS", () => {
		const mutated = removeEdge(ROWS, "REL-CLOSE", "REL-DOCS");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-RELDOCS: required edge REL-CLOSE blocked_by REL-DOCS missing (actual: [REL-T4, REL-T5, REL-T6, REL-T7, REL-T8, REL-T9, REL-R2])",
		);
	});

	test("removing the DOC-F<-REL-DOCS edge fails G-RELDOCS", () => {
		const mutated = removeEdge(ROWS, "DOC-F", "REL-DOCS");
		const violations = checkGateEdges(mutated);
		expect(violations).toContain(
			"G-RELDOCS: required edge DOC-F blocked_by REL-DOCS missing (actual: [PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-D, DOC-E])",
		);
	});
});

// ============================================================================
// Guarded set non-vacuity
// ============================================================================

describe("guarded set non-vacuity (MAP-4)", () => {
	test("every gate has a non-empty guarded set on the published registry", () => {
		expect(checkGuardedNonVacuous(ROWS)).toEqual([]);
	});

	test("G-FREEZE guards DEPS-R1 and downstream DEPS nodes", () => {
		const guarded = computeGuardedSet(ROWS, GATES[0]!);
		expect(guarded).toContain("DEPS-R1");
		expect(guarded).toContain("DEPS-T1");
		expect(guarded).toContain("DEPS-D1");
	});

	test("G-RELDOCS guards REL-CLOSE and DOC-F", () => {
		const reldocsGate = GATES.find((g) => g.id === "G-RELDOCS")!;
		const guarded = computeGuardedSet(ROWS, reldocsGate);
		expect(guarded).toContain("REL-CLOSE");
		expect(guarded).toContain("DOC-F");
	});

	test("G-ALLTRACKS guards MAP-5", () => {
		const alltracksGate = GATES.find((g) => g.id === "G-ALLTRACKS")!;
		const guarded = computeGuardedSet(ROWS, alltracksGate);
		expect(guarded).toContain("MAP-5");
	});
});

// ============================================================================
// Gate predicate — simulated out-of-order status
// ============================================================================

describe("gate predicate evaluation (MAP-4)", () => {
	test("all gates pass when every node is resolved", () => {
		const allResolved = statusesExcept([]);
		expect(evaluateAllGates(ROWS, GATES, allResolved)).toEqual([]);
	});

	test("all gates pass when every node is open", () => {
		const allOpen = statusesFor([]);
		expect(evaluateAllGates(ROWS, GATES, allOpen)).toEqual([]);
	});

	test("a DEPS node resolved before PAR-CLOSE fails G-FREEZE", () => {
		// DEPS-R1 resolved, PAR-CLOSE open
		const statuses = statusesFor(["DEPS-R1"]);
		const violations = evaluateGate(ROWS, GATES[0]!, statuses);
		expect(violations).toContain(
			"G-FREEZE: DEPS-R1 resolved before trigger(s) PAR-CLOSE resolved",
		);
	});

	test("a DEPS node marked ready before PAR-CLOSE resolves fails the full evaluation", () => {
		// Simulate: DEPS-R1 and its downstream are resolved, but PAR-CLOSE is still open
		const statuses = statusesFor(["DEPS-R1", "DEPS-B1", "DEPS-B2", "DEPS-X1"]);
		const violations = evaluateAllGates(ROWS, GATES, statuses);
		expect(violations.length).toBeGreaterThan(0);
		expect(violations.some((v) => v.includes("G-FREEZE") && v.includes("DEPS-R1"))).toBe(true);
	});

	test("REL-CLOSE resolved while REL-DOCS is open fails G-RELDOCS", () => {
		const reldocsGate = GATES.find((g) => g.id === "G-RELDOCS")!;
		const statuses = statusesFor(["REL-CLOSE"]);
		const violations = evaluateGate(ROWS, reldocsGate, statuses);
		expect(violations).toContain(
			"G-RELDOCS: REL-CLOSE resolved before trigger(s) REL-DOCS resolved",
		);
	});

	test("DOC-F resolved while REL-DOCS is open fails G-RELDOCS", () => {
		const reldocsGate = GATES.find((g) => g.id === "G-RELDOCS")!;
		const statuses = statusesFor(["DOC-F"]);
		const violations = evaluateGate(ROWS, reldocsGate, statuses);
		expect(violations).toContain(
			"G-RELDOCS: DOC-F resolved before trigger(s) REL-DOCS resolved",
		);
	});

	test("PAR-CLOSE resolved while XC-2 is open fails G-MIRROR", () => {
		const mirrorGate = GATES.find((g) => g.id === "G-MIRROR")!;
		const statuses = statusesFor(["PAR-CLOSE"]);
		const violations = evaluateGate(ROWS, mirrorGate, statuses);
		expect(violations).toContain(
			"G-MIRROR: PAR-CLOSE resolved before trigger(s) XC-2 resolved",
		);
	});

	test("TUI-V1 resolved while EXT-26 is open fails G-RELINFRA", () => {
		const relinfraGate = GATES.find((g) => g.id === "G-RELINFRA")!;
		const statuses = statusesFor(["TUI-V1"]);
		const violations = evaluateGate(ROWS, relinfraGate, statuses);
		expect(violations).toContain(
			"G-RELINFRA: TUI-V1 resolved before trigger(s) EXT-26 resolved",
		);
	});

	test("XC-CLOSE resolved while PAR-CLOSE is open fails G-PARCLOSE", () => {
		const parcloseGate = GATES.find((g) => g.id === "G-PARCLOSE")!;
		const statuses = statusesFor(["XC-CLOSE"]);
		const violations = evaluateGate(ROWS, parcloseGate, statuses);
		expect(violations).toContain(
			"G-PARCLOSE: XC-CLOSE resolved before trigger(s) PAR-CLOSE resolved",
		);
	});

	test("DOC-F resolved while DEPS-D1 is open fails G-DEPCLOSE", () => {
		const depcloseGate = GATES.find((g) => g.id === "G-DEPCLOSE")!;
		const statuses = statusesFor(["DOC-F"]);
		const violations = evaluateGate(ROWS, depcloseGate, statuses);
		expect(violations).toContain(
			"G-DEPCLOSE: DOC-F resolved before trigger(s) DEPS-D1 resolved",
		);
	});

	test("MAP-5 resolved while a closer is open fails G-ALLTRACKS", () => {
		const alltracksGate = GATES.find((g) => g.id === "G-ALLTRACKS");
		if (alltracksGate === undefined) throw new Error("G-ALLTRACKS not found");
		// All closers resolved except PAR-CLOSE
		const closers = ["XC-CLOSE", "TUI-CLOSE", "PERF-CLOSE", "REL-CLOSE", "DEPS-D1", "DOC-F", "ARC-CLOSE", "MAP-5"];
		const statuses = statusesFor(closers);
		const violations = evaluateGate(ROWS, alltracksGate, statuses);
		expect(violations).toContain(
			"G-ALLTRACKS: MAP-5 resolved before trigger(s) PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, ARC-CLOSE resolved",
		);
	});

	test("a valid topological prefix passes all gates", () => {
		// Resolve EXT-14, EXT-15, EXT-21, EXT-23, EXT-24, EXT-25, EXT-26, VER-ALIGN, PAR-LEDGER
		// These are all zero-blocker or early nodes — no gate should fire
		const statuses = statusesFor([
			"EXT-14", "EXT-15", "EXT-21", "EXT-23", "EXT-24", "EXT-25", "EXT-26",
			"VER-ALIGN", "PAR-LEDGER", "PERF-R18", "REL-R1", "REL-R2", "REL-R3",
			"TUI-R1", "TUI-R2", "DEPS-R3",
		]);
		expect(evaluateAllGates(ROWS, GATES, statuses)).toEqual([]);
	});
});

// ============================================================================
// Dependency-law checks
// ============================================================================

describe("dependency-law edges (MAP-4)", () => {
	test("every dependency-law transitive prerequisite holds", () => {
		expect(checkDependencyLaws(ROWS)).toEqual([]);
	});

	test("DEPS-T1 transitively depends on PAR-CLOSE and EXT-23", () => {
		const closure = prerequisiteClosure(ROWS, "DEPS-T1");
		expect(closure.has("PAR-CLOSE")).toBe(true);
		expect(closure.has("EXT-23")).toBe(true);
	});

	test("DEPS-R1 transitively depends on EXT-26", () => {
		const closure = prerequisiteClosure(ROWS, "DEPS-R1");
		expect(closure.has("EXT-26")).toBe(true);
	});

	test("a missing transitive prerequisite fails", () => {
		// Remove the PAR-CLOSE edge from DEPS-R1, breaking the transitive chain
		const mutated = removeEdge(ROWS, "DEPS-R1", "PAR-CLOSE");
		const violations = checkDependencyLaws(mutated);
		expect(violations).toContain(
			"DL-EPOCH: DEPS-T1 does not transitively depend on PAR-CLOSE (dependency-law edge missing)",
		);
	});
});

// ============================================================================
// Track-start ordering
// ============================================================================

describe("track-start ordering (MAP-4)", () => {
	test("no track-start edge contradicts the wayfinder ordering", () => {
		expect(checkTrackOrdering(ROWS)).toEqual([]);
	});

	test("parity is before dependency (no PAR node depends on DEPS)", () => {
		const parNodes = ROWS.filter((r) => r.stableId.startsWith("PAR-"));
		for (const par of parNodes) {
			const closure = prerequisiteClosure(ROWS, par.stableId);
			for (const row of ROWS) {
				if (row.stableId.startsWith("DEPS-")) {
					expect(closure.has(row.stableId)).toBe(false);
				}
			}
		}
	});

	test("extension mirror is before parity ratification (XC-2 does not depend on PAR-CLOSE)", () => {
		const closure = prerequisiteClosure(ROWS, "XC-2");
		expect(closure.has("PAR-CLOSE")).toBe(false);
	});

	test("release infrastructure is before terminal proof (EXT-26 does not depend on TUI-V1)", () => {
		const closure = prerequisiteClosure(ROWS, "EXT-26");
		expect(closure.has("TUI-V1")).toBe(false);
	});

	test("documentation staging is before release/doc closure (REL-DOCS does not depend on REL-CLOSE or DOC-F)", () => {
		const closure = prerequisiteClosure(ROWS, "REL-DOCS");
		expect(closure.has("REL-CLOSE")).toBe(false);
		expect(closure.has("DOC-F")).toBe(false);
	});
});

// ============================================================================
// Graph re-pass
// ============================================================================

describe("graph re-pass (MAP-4)", () => {
	test("the MAP-1 graph check is green on the published records", () => {
		expect(runMapLedgerChecks(INPUTS)).toEqual([]);
	});
});

// ============================================================================
// Full orchestration
// ============================================================================

describe("full orchestration (MAP-4)", () => {
	test("runGatesChecks returns zero violations on the published tree", () => {
		const violations = runGatesChecks({ mapLedger: INPUTS });
		expect(violations).toEqual([]);
	});

	test("loadGatesInputs reads the published artifacts", () => {
		const inputs = loadGatesInputs(REPO_ROOT);
		expect(inputs.mapLedger.snapshotText.length).toBeGreaterThan(0);
		expect(inputs.mapLedger.mapText.length).toBeGreaterThan(0);
	});

	test("the CLI prints GATES_OK on the published tree", () => {
		const result = Bun.spawnSync(["bun", "run", "scripts/verification/gates.ts"], {
			stdout: "pipe",
			stderr: "pipe",
			cwd: REPO_ROOT,
		});
		const stdout = new TextDecoder().decode(result.stdout);
		const stderr = new TextDecoder().decode(result.stderr);
		expect(result.exitCode).toBe(0);
		expect(stdout).toContain("GATES_OK");
		expect(stderr).toBe("");
	});
});
