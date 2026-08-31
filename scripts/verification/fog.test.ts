import { describe, expect, test } from "bun:test";
import {
	SNAPSHOT_STRUCTURAL_SHA256,
	loadMapLedgerInputs,
	parseSnapshot,
	type Snapshot,
} from "./map.ts";
import { REPO_ROOT } from "./parity.ts";
import {
	DECISION_INDEX_MARKER,
	checkCloseGate,
	checkDecisionIndex,
	checkFogGraduation,
	checkSingleIndexComment,
	dryRunStatusFlip,
	findDecisionIndexComment,
	flipStatus,
	graduationCensus,
	runFogLintChecks,
	type IssueStates,
} from "./fog.ts";

const INPUTS = loadMapLedgerInputs(REPO_ROOT);
const SNAPSHOT = parseSnapshot(INPUTS.snapshotText).snapshot as Snapshot;

function statesFor(openIssues: readonly number[]): IssueStates {
	return new Map(
		SNAPSHOT.records.map((record) => [record.issue, openIssues.includes(record.issue) ? "open" : "closed"]),
	);
}
/** Rewrite one record inside the published witness text (mutation helper). */
function editRecord(stableId: string, edit: (record: { acceptance: string | null; question: string | null }) => void): string {
	const parsed = JSON.parse(INPUTS.snapshotText) as { records: Array<{ stableId: string; acceptance: string | null; question: string | null }> };
	const record = parsed.records.find((candidate) => candidate.stableId === stableId);
	if (record === undefined) throw new Error(`no record ${stableId}`);
	edit(record);
	return `${JSON.stringify(parsed, null, 2)}\n`;
}

/** Render a synthetic in-coverage index comment for the given states. */
function renderIndex(states: IssueStates, extraRows: readonly string[] = [], dropId?: string): string {
	const rows = SNAPSHOT.records
		.filter((record) => states.get(record.issue) === "closed" && record.stableId !== dropId)
		.map((record) => `| ${record.stableId} | settled | rejected option rationale |`);
	return [
		DECISION_INDEX_MARKER,
		"",
		"| Stable ID | Decision | Rejected option |",
		"| --- | --- | --- |",
		...rows,
		...extraRows.map((row) => `| ${row} | settled | rejected option rationale |`),
	].join("\n");
}

describe("fog graduation (MAP-3)", () => {
	test("published records carry zero ungraduated shipped-surface fog nodes", () => {
		expect(checkFogGraduation(SNAPSHOT)).toEqual([]);
	});

	test("externals with null acceptance are prerequisite research, not fog", () => {
		const externals = SNAPSHOT.records.filter((record) => record.kind === "external");
		expect(externals.length).toBe(16);
		expect(checkFogGraduation(SNAPSHOT).filter((v) => v.startsWith("EXT-"))).toEqual([]);
	});

	test("an execution record published without an acceptance contract is ungraduated fog", () => {
		const mutated = parseSnapshot(editRecord("MAP-3", (record) => (record.acceptance = null)));
		expect(mutated.snapshot).not.toBeNull();
		expect(checkFogGraduation(mutated.snapshot as Snapshot)).toEqual([
			"MAP-3: shipped-surface node publishes no acceptance contract — ungraduated fog",
		]);
	});

	test("an execution record published without a question is ungraduated fog", () => {
		const mutated = parseSnapshot(editRecord("PAR-LEDGER", (record) => (record.question = "  ")));
		expect(mutated.snapshot).not.toBeNull();
		expect(checkFogGraduation(mutated.snapshot as Snapshot)).toEqual([
			"PAR-LEDGER: shipped-surface node publishes no question — ungraduated fog",
		]);
	});
});

describe("graduation census (MAP-3)", () => {
	test("a fully closed shipped surface graduates every execution node", () => {
		const all = statesFor([]);
		const census = graduationCensus(SNAPSHOT, all);
		expect(census.open).toEqual([]);
		expect(census.unknownState).toEqual([]);
		expect(census.graduated).toHaveLength(143);
		expect(census.closedDecisions).toHaveLength(159);
	});

	test("open execution nodes are the ungraduated frontier; open externals are not", () => {
		const census = graduationCensus(SNAPSHOT, statesFor([141, 13]));
		expect(census.open).toEqual(["MAP-3"]);
		expect(census.graduated).toHaveLength(142);
		expect(census.closedDecisions).toHaveLength(157);
		expect(census.closedDecisions).not.toContain("MAP-3");
	});

	test("a record without a live state cannot be classified", () => {
		const states = new Map(statesFor([]));
		states.delete(141);
		const census = graduationCensus(SNAPSHOT, states);
		expect(census.unknownState).toEqual(["MAP-3"]);
	});
});

describe("decision index coverage (MAP-3)", () => {
	test("the index covers exactly the closed records", () => {
		const states = statesFor([141, 30]);
		expect(checkDecisionIndex(renderIndex(states), SNAPSHOT, states)).toEqual([]);
	});

	test("a closed decision missing from the index fails coverage", () => {
		const states = statesFor([]);
		const violations = checkDecisionIndex(renderIndex(states, [], "EXT-13"), SNAPSHOT, states);
		expect(violations).toEqual(["EXT-13: closed decision is missing from the index"]);
	});

	test("a phantom decision row fails coverage", () => {
		const states = statesFor([]);
		const violations = checkDecisionIndex(renderIndex(states, ["GHOST-1"]), SNAPSHOT, states);
		expect(violations).toEqual(["GHOST-1: indexed decision matches no registry record (phantom decision row)"]);
	});

	test("a row naming a still-open ticket is a premature decision", () => {
		const states = statesFor([141]);
		const violations = checkDecisionIndex(renderIndex(states, ["MAP-3"]), SNAPSHOT, states);
		expect(violations).toEqual([
			"MAP-3: index row names still-open ticket #141; only closed decisions belong",
		]);
	});

	test("a duplicated index row violates the append-only one-row-per-decision rule", () => {
		const states = statesFor([]);
		const violations = checkDecisionIndex(renderIndex(states, ["EXT-13"]), SNAPSHOT, states);
		expect(violations).toEqual(["EXT-13: indexed 2 times; the append-only index carries exactly one row per decision"]);
	});

	test("a second marker comment fragments the index and fails the lint", () => {
		const comments = [renderIndex(statesFor([])), renderIndex(statesFor([]))];
		expect(checkSingleIndexComment(comments)).toEqual([
			"canonical issue carries 2 Decisions-so-far index comments; exactly one append-only index comment is allowed",
		]);
		expect(checkSingleIndexComment(["one index only", renderIndex(statesFor([]))])).toEqual([]);
	});

	test("an absent index comment is itself a violation", () => {
		const states = statesFor([]);
		expect(checkDecisionIndex(null, SNAPSHOT, states)).toEqual([
			"canonical issue carries no Decisions-so-far index comment",
		]);
	});

	test("the marker locates the index comment among issue comments", () => {
		const comments = ["unrelated review note", renderIndex(statesFor([])), "later chatter"];
		expect(findDecisionIndexComment(comments)).toContain(DECISION_INDEX_MARKER);
		expect(findDecisionIndexComment(["no marker here"])).toBeNull();
	});
});

describe("dry-run status flip (MAP-3)", () => {
	test("flipping an open node re-runs the graph check against unchanged records", () => {
		const states = statesFor([141, 30]);
		const flip = dryRunStatusFlip(INPUTS, SNAPSHOT, states, "MAP-3");
		expect(flip.flipped).toBe("MAP-3 (#141): open -> closed (dry-run)");
		expect(flip.graphViolations).toEqual([]);
		expect(flip.aliasViolations).toEqual([]);
		expect(flip.relDocsBypassViolations).toEqual([]);
		expect(flip.structuralSha256).toBe(SNAPSHOT_STRUCTURAL_SHA256);
		expect(flip.census.open).toEqual(["PAR-WIRE"]);
		expect(flip.census.graduated).toHaveLength(142);
	});

	test("flipping a closed node (reopen simulation) leaves the published graph green", () => {
		const states = statesFor([]);
		const flip = dryRunStatusFlip(INPUTS, SNAPSHOT, states, "EXT-13");
		expect(flip.flipped).toBe("EXT-13 (#13): closed -> open (dry-run)");
		expect(flip.graphViolations).toEqual([]);
		expect(flip.aliasViolations.length).toBe(0);
		expect(flip.relDocsBypassViolations.length).toBe(0);
		expect(flip.census.closedDecisions).toHaveLength(158);
	});

	test("a flip that perturbs the records fails the re-run loudly", () => {
		const tampered = { ...INPUTS, mapText: INPUTS.mapText.replace("| MAP-3 | task |", "| EXT-PARITY | task |") };
		const flip = dryRunStatusFlip(tampered, SNAPSHOT, statesFor([141]), "MAP-3");
		expect(flip.graphViolations.length).toBeGreaterThan(0);
		expect(flip.aliasViolations.length).toBeGreaterThan(0);
	});

	test("flipStatus inverts one issue and rejects unknown issues", () => {
		const flipped = flipStatus(statesFor([141]), 141);
		expect(flipped.get(141)).toBe("closed");
		expect(flipStatus(flipped, 141).get(141)).toBe("open");
		expect(() => flipStatus(statesFor([]), 99999)).toThrow("cannot flip unknown issue #99999");
	});

	test("unknown stable IDs cannot be flipped", () => {
		expect(() => dryRunStatusFlip(INPUTS, SNAPSHOT, statesFor([]), "NOPE-1")).toThrow(
			"unknown stable ID 'NOPE-1'",
		);
	});

	test("the CLI rejects --flip without a stable ID instead of no-oping green", () => {
		const result = Bun.spawnSync(["bun", "run", "scripts/verification/fog.ts", "--flip"], {
			stdout: "pipe",
			stderr: "pipe",
			cwd: REPO_ROOT,
		});
		expect(result.exitCode).toBe(2);
		expect(new TextDecoder().decode(result.stderr)).toContain("--flip needs a stable ID");
	});
});

describe("closure gate and orchestration (MAP-3)", () => {
	test("the closure gate is red while any shipped-surface fog node is ungraduated", () => {
		const census = graduationCensus(SNAPSHOT, statesFor([141, 30, 144]));
		expect(checkCloseGate(census)).toEqual([
			"[close-gate] 3 shipped-surface fog node(s) remain ungraduated: PAR-WIRE, MAP-3, MAP-5",
		]);
	});

	test("the closure gate is green on a fully graduated shipped surface", () => {
		expect(checkCloseGate(graduationCensus(SNAPSHOT, statesFor([])))).toEqual([]);
	});

	test("the full lint is green on graduated records, a green graph, and an exact index", () => {
		const states = statesFor([141, 30]);
		const { violations, census } = runFogLintChecks(INPUTS, states, renderIndex(states));
		expect(violations).toEqual([]);
		expect(census.open).toEqual(["PAR-WIRE", "MAP-3"]);
	});

	test("the lint reports a missing index row alongside a green graph", () => {
		const states = statesFor([]);
		const { violations } = runFogLintChecks(INPUTS, states, renderIndex(states, [], "MAP-1"));
		expect(violations).toEqual(["[decision-index] MAP-1: closed decision is missing from the index"]);
	});
});
