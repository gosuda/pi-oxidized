import { describe, expect, test } from "bun:test";
import {
	loadMapLedgerInputs,
	parseExecutionMap,
	runMapLedgerChecks,
	type MapLedgerInputs,
	type MapRow,
} from "./map.ts";
import { REPO_ROOT } from "./parity.ts";
import {
	ARBITRATION_DOC_PATH,
	OWNERSHIP_SURFACES,
	RULING_COUNT,
	RULINGS,
	checkBindingEdges,
	checkOwners,
	checkOwnershipUniqueness,
	checkRulingCount,
	runArbitrationChecks,
} from "./arbitration.ts";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const INPUTS = loadMapLedgerInputs(REPO_ROOT);
const DOC = parseExecutionMap(INPUTS.mapText);
const ARBITRATION_DOC = readFileSync(join(REPO_ROOT, ARBITRATION_DOC_PATH), "utf8");

/** Remove a prerequisite from a row's blocked_by list (mutation helper). */
function removeEdge(rows: readonly MapRow[], dependent: string, prerequisite: string): MapRow[] {
	return rows.map((row) =>
		row.stableId === dependent
			? { ...row, blockedBy: row.blockedBy.filter((b) => b !== prerequisite) }
			: row,
	);
}

describe("ruling count (MAP-2)", () => {
	test("exactly fifteen rulings are published", () => {
		expect(checkRulingCount(ARBITRATION_DOC)).toEqual([]);
	});

	test("a doc with fourteen rulings fails", () => {
		const edited = ARBITRATION_DOC.replace("| AR15 |", "| ARXX |");
		expect(checkRulingCount(edited)).toEqual([
			`${ARBITRATION_DOC_PATH} publishes 14 ruling(s), expected ${RULING_COUNT}`,
		]);
	});

	test("a doc with a duplicate ruling ID fails", () => {
		const edited = ARBITRATION_DOC.replace("| AR15 |", "| AR1 |");
		const violations = checkRulingCount(edited);
		expect(violations.length).toBeGreaterThan(0);
	});
});

describe("owners (MAP-2)", () => {
	test("every ruling owner exists in the registry", () => {
		expect(checkOwners(DOC.rows)).toEqual([]);
	});

	test("a ruling with a nonexistent owner fails", () => {
		const badRulings = [{ ...RULINGS[0]!, owner: "NONEXISTENT", ownerIssue: 999 }];
		expect(checkOwners(DOC.rows, badRulings)).toEqual([
			"AR1: owner NONEXISTENT not found in registry",
		]);
	});

	test("a ruling with a wrong issue number fails", () => {
		const badRulings = [{ ...RULINGS[0]!, ownerIssue: 999 }];
		expect(checkOwners(DOC.rows, badRulings)).toEqual([
			"AR1: owner XC-2 has issue #41, expected #999",
		]);
	});
});

describe("binding edges (MAP-2)", () => {
	test("every binding edge is present in the registry", () => {
		expect(checkBindingEdges(DOC.rows)).toEqual([]);
	});

	test("removing the PAR-CLOSE<-XC-2 edge fails", () => {
		const mutated = removeEdge(DOC.rows, "PAR-CLOSE", "XC-2");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR1: binding edge PAR-CLOSE blocked_by XC-2 missing (actual: [PAR-FOLD, PAR-CLIENT, PAR-SERVER, PAR-COMPAT-AUDIT, PAR-COMPAT-DISPO, PAR-PTY-GRILL])",
		);
	});

	test("removing the DOC-A<-VER-ALIGN edge fails", () => {
		const mutated = removeEdge(DOC.rows, "DOC-A", "VER-ALIGN");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR8: binding edge DOC-A blocked_by VER-ALIGN missing (actual: [EXT-24])",
		);
	});

	test("removing the DOC-F<-DEPS-D1 edge fails", () => {
		const mutated = removeEdge(DOC.rows, "DOC-F", "DEPS-D1");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR10: binding edge DOC-F blocked_by DEPS-D1 missing (actual: [PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, REL-DOCS, DOC-D, DOC-E])",
		);
	});

	test("removing the REL-CLOSE<-REL-DOCS edge fails", () => {
		const mutated = removeEdge(DOC.rows, "REL-CLOSE", "REL-DOCS");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR5: binding edge REL-CLOSE blocked_by REL-DOCS missing (actual: [REL-T4, REL-T5, REL-T6, REL-T7, REL-T8, REL-T9, REL-R2])",
		);
	});

	test("removing the TUI-V1<-EXT-26 edge fails", () => {
		const mutated = removeEdge(DOC.rows, "TUI-V1", "EXT-26");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR11: binding edge TUI-V1 blocked_by EXT-26 missing (actual: [TUI-P1, TUI-T1, TUI-T5])",
		);
	});

	test("removing the TUI-V1<-TUI-P1 edge fails", () => {
		const mutated = removeEdge(DOC.rows, "TUI-V1", "TUI-P1");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR12: binding edge TUI-V1 blocked_by TUI-P1 missing (actual: [EXT-26, TUI-T1, TUI-T5])",
		);
	});

	test("removing the XC-CLOSE<-PAR-CLOSE edge fails", () => {
		const mutated = removeEdge(DOC.rows, "XC-CLOSE", "PAR-CLOSE");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR14: binding edge XC-CLOSE blocked_by PAR-CLOSE missing (actual: [XC-2, XC-3, XC-4, XC-5, XC-6, XC-7, XC-8, XC-9])",
		);
	});

	test("removing a MAP-5<-closer edge fails", () => {
		const mutated = removeEdge(DOC.rows, "MAP-5", "PAR-CLOSE");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR15: binding edge MAP-5 blocked_by PAR-CLOSE missing (actual: [MAP-4, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E])",
		);
	});

	test("removing the DOC-F<-REL-DOCS edge fails", () => {
		const mutated = removeEdge(DOC.rows, "DOC-F", "REL-DOCS");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR5: binding edge DOC-F blocked_by REL-DOCS missing (actual: [PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-D, DOC-E])",
		);
	});

	test("removing the DOC-F<-REL-CLOSE edge fails", () => {
		const mutated = removeEdge(DOC.rows, "DOC-F", "REL-CLOSE");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR5: binding edge DOC-F blocked_by REL-CLOSE missing (actual: [PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-DOCS, DEPS-D1, DOC-D, DOC-E])",
		);
	});

	test("removing the DOC-E<-REL-CLOSE edge fails", () => {
		const mutated = removeEdge(DOC.rows, "DOC-E", "REL-CLOSE");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR9: binding edge DOC-E blocked_by REL-CLOSE missing (actual: [DOC-B, DOC-C])",
		);
	});

	test("removing the MAP-5<-XC-CLOSE edge fails", () => {
		const mutated = removeEdge(DOC.rows, "MAP-5", "XC-CLOSE");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR15: binding edge MAP-5 blocked_by XC-CLOSE missing (actual: [MAP-4, PAR-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E])",
		);
	});

	test("removing the MAP-5<-TUI-CLOSE edge fails", () => {
		const mutated = removeEdge(DOC.rows, "MAP-5", "TUI-CLOSE");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR15: binding edge MAP-5 blocked_by TUI-CLOSE missing (actual: [MAP-4, PAR-CLOSE, XC-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E])",
		);
	});

	test("removing the MAP-5<-PERF-CLOSE edge fails", () => {
		const mutated = removeEdge(DOC.rows, "MAP-5", "PERF-CLOSE");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR15: binding edge MAP-5 blocked_by PERF-CLOSE missing (actual: [MAP-4, PAR-CLOSE, XC-CLOSE, TUI-CLOSE, REL-CLOSE, DEPS-D1, DOC-F, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E])",
		);
	});

	test("removing the MAP-5<-REL-CLOSE edge fails", () => {
		const mutated = removeEdge(DOC.rows, "MAP-5", "REL-CLOSE");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR15: binding edge MAP-5 blocked_by REL-CLOSE missing (actual: [MAP-4, PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, DEPS-D1, DOC-F, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E])",
		);
	});

	test("removing the MAP-5<-DEPS-D1 edge fails", () => {
		const mutated = removeEdge(DOC.rows, "MAP-5", "DEPS-D1");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR15: binding edge MAP-5 blocked_by DEPS-D1 missing (actual: [MAP-4, PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DOC-F, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E])",
		);
	});

	test("removing the MAP-5<-DOC-F edge fails", () => {
		const mutated = removeEdge(DOC.rows, "MAP-5", "DOC-F");
		const violations = checkBindingEdges(mutated);
		expect(violations).toContain(
			"AR15: binding edge MAP-5 blocked_by DOC-F missing (actual: [MAP-4, PAR-CLOSE, XC-CLOSE, TUI-CLOSE, PERF-CLOSE, REL-CLOSE, DEPS-D1, REL-R2, DEPS-R3, DEPS-R2, DEPS-G1, DOC-D, DOC-E])",
		);
	});
});

describe("ownership uniqueness (MAP-2)", () => {
	test("no surface has two owners", () => {
		expect(checkOwnershipUniqueness(DOC.rows)).toEqual([]);
	});

	test("a surface with a nonexistent owner fails", () => {
		const badSurfaces = [{ name: "test", owner: "NONEXISTENT" }];
		expect(checkOwnershipUniqueness(DOC.rows, badSurfaces)).toEqual([
			"ownership surface 'test': owner NONEXISTENT not found in registry",
		]);
	});

	test("duplicate owners across surfaces fail", () => {
		const badSurfaces = [
			{ name: "surface-a", owner: "XC-2" },
			{ name: "surface-b", owner: "XC-2" },
		];
		const violations = checkOwnershipUniqueness(DOC.rows, badSurfaces);
		expect(violations).toContain("ownership surfaces have duplicate owners: XC-2");
	});
});

describe("graph re-pass (MAP-2)", () => {
	test("the MAP-1 graph check is green on the published records", () => {
		expect(runMapLedgerChecks(INPUTS)).toEqual([]);
	});
});

describe("full orchestration (MAP-2)", () => {
	test("runArbitrationChecks returns zero violations on the published tree", () => {
		const violations = runArbitrationChecks({
			mapLedger: INPUTS,
			arbitrationDoc: ARBITRATION_DOC,
		});
		expect(violations).toEqual([]);
	});
});
