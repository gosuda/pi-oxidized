import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
	EXPECTED_AXES,
	LEDGER_PATH,
	PENDING_MARKER_RE,
	REPO_ROOT,
	parseLedgerRows,
	parseWitnessCell,
	runStyleLedgerWitnesses,
	splitRowCells,
	verifyAxisSet,
	verifyCitedLiterals,
	verifyNoPendingMarkers,
	verifyOneDecisionPerAxis,
} from "./style-ledger.ts";

const LEDGER_TEXT = readFileSync(join(REPO_ROOT, LEDGER_PATH), "utf8");

/** A minimal ten-row ledger whose witness literals need not exist on disk. */
function fixtureRows(): string[] {
	return EXPECTED_AXES.map(
		(axis) =>
			`| ${axis.id} | ${axis.title} | decision-${axis.id} | \`ref-${axis.id}.ts:1\` \`L-${axis.id}\` | \`rust-${axis.id}.rs:1\` \`R-${axis.id}\` | TUI-T${axis.id} |`,
	);
}

const FIXTURE = `| # | Axis | Pinned decision | Reference witness | Rust witness | Applying ticket |
|---|------|-----------------|-------------------|--------------|-----------------|
${fixtureRows().join("\n")}
`;

describe("style-ledger witness suite", () => {
	test("declares exactly the ten expected axes in order", () => {
		expect(EXPECTED_AXES).toHaveLength(10);
		expect(EXPECTED_AXES.map((axis) => axis.id)).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
	});

	test("full witness run is green against the real ledger", () => {
		expect(runStyleLedgerWitnesses(REPO_ROOT)).toEqual([]);
	});
});

describe("ledger parsing", () => {
	test("parses exactly ten axis rows from the real ledger", () => {
		const { rows, problems } = parseLedgerRows(LEDGER_TEXT);
		expect(problems).toEqual([]);
		expect(rows).toHaveLength(10);
		expect(rows.map((row) => row.id)).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
	});

	test("parses a witness cell into path, line, and literal", () => {
		expect(parseWitnessCell("`crates/pi/src/cli/bootstrap.rs:491` `Error: {message}`")).toEqual({
			path: "crates/pi/src/cli/bootstrap.rs",
			line: 491,
			literal: "Error: {message}",
		});
	});

	test("returns undefined for a witness cell without a path:line span", () => {
		expect(parseWitnessCell("`just a literal`")).toBeUndefined();
		expect(parseWitnessCell("no spans at all")).toBeUndefined();
	});

	test("returns undefined when a witness cell has more than one literal span", () => {
		expect(parseWitnessCell("`a.ts:1` `one` `two`")).toBeUndefined();
	});

	test("splits table cells and unescapes escaped pipes", () => {
		expect(splitRowCells("| 4 | A | `a \\|\\| b` | `x.ts:1` `lit` | B |")).toEqual([
			"4",
			"A",
			"`a || b`",
			"`x.ts:1` `lit`",
			"B",
		]);
	});
});

describe("verifyAxisSet", () => {
	test("passes on the real ledger", () => {
		expect(verifyAxisSet(LEDGER_TEXT)).toEqual([]);
	});

	test("passes on the synthetic fixture", () => {
		expect(verifyAxisSet(FIXTURE)).toEqual([]);
	});

	test("fails when an axis row is dropped", () => {
		const mutated = FIXTURE.replace(/\n\| 10 \| [^\n]+/, "");
		expect(verifyAxisSet(mutated)).not.toEqual([]);
	});

	test("fails when an axis id drifts from its expected slot", () => {
		const mutated = FIXTURE.replace("| 10 | Onboarding and consent |", "| 99 | Onboarding and consent |");
		expect(verifyAxisSet(mutated).some((problem) => problem.includes("id 99"))).toBe(true);
	});

	test("fails when an axis title is renamed", () => {
		const mutated = FIXTURE.replace("| 1 | Case |", "| 1 | Casing |");
		expect(verifyAxisSet(mutated).some((problem) => problem.includes('"Casing"'))).toBe(true);
	});
});

describe("verifyOneDecisionPerAxis", () => {
	test("passes on the real ledger", () => {
		expect(verifyOneDecisionPerAxis(LEDGER_TEXT)).toEqual([]);
	});

	test("fails when a Pinned decision is emptied", () => {
		const mutated = FIXTURE.replace("decision-1", "");
		expect(verifyOneDecisionPerAxis(mutated).some((problem) => problem.includes("empty Pinned decision"))).toBe(
			true,
		);
	});

	test("fails when a Pinned decision carries an unresolved marker", () => {
		const mutated = FIXTURE.replace("decision-2", "decision-2 TBD");
		expect(
			verifyOneDecisionPerAxis(mutated).some((problem) => problem.includes("unresolved marker")),
		).toBe(true);
	});

	test("fails when a reference witness cell is malformed", () => {
		const mutated = FIXTURE.replace("`ref-1.ts:1` `L-1`", "`no-site` `L-1`");
		expect(verifyOneDecisionPerAxis(mutated).some((problem) => problem.includes("reference"))).toBe(true);
	});

	test("fails when an applying ticket is left blank", () => {
		const mutated = FIXTURE.replace("| TUI-T3 |", "|  |");
		expect(verifyOneDecisionPerAxis(mutated).some((problem) => problem.includes("no applying ticket"))).toBe(true);
	});
});

describe("PENDING_MARKER_RE", () => {
	test.each(["TBD", "pending", "UNDECIDED", "unresolved", "MAYBE", "TODO", "FIXME", "candidate"])(
		"flags %s as an unresolved token",
		(token) => {
			expect(PENDING_MARKER_RE.test(token)).toBe(true);
		},
	);

	test("does not flag legitimate UI vocabulary", () => {
		expect(PENDING_MARKER_RE.test("placeholder text")).toBe(false);
	});
});

describe("verifyNoPendingMarkers", () => {
	test("passes on the real ledger", () => {
		expect(verifyNoPendingMarkers(LEDGER_TEXT)).toEqual([]);
	});

	test("fails when a marker token appears anywhere in the ledger", () => {
		const mutated = LEDGER_TEXT.replace("U+2026", "PENDING U+2026");
		expect(verifyNoPendingMarkers(mutated)).not.toEqual([]);
	});
});

describe("verifyCitedLiterals", () => {
	test("passes on the real ledger and repository", () => {
		expect(verifyCitedLiterals(REPO_ROOT, LEDGER_TEXT)).toEqual([]);
	});

	test("fails when a cited literal no longer exists in its file", () => {
		const mutated = LEDGER_TEXT.replace(
			"`crates/pi/src/cli/bootstrap.rs:503` `Error: {message}`",
			"`crates/pi/src/cli/bootstrap.rs:503` `No Such Literal Anymore`",
		);
		expect(verifyCitedLiterals(REPO_ROOT, mutated)).not.toEqual([]);
	});

	test("fails when a witness line number drifts", () => {
		const mutated = LEDGER_TEXT.replace(
			"crates/pi/src/cli/bootstrap.rs:503",
			"crates/pi/src/cli/bootstrap.rs:1",
		);
		expect(verifyCitedLiterals(REPO_ROOT, mutated)).not.toEqual([]);
	});

	test("fails when a cited path is unreadable", () => {
		const mutated = LEDGER_TEXT.replace(
			"crates/pi/src/cli/bootstrap.rs:503",
			"crates/pi/src/cli/missing-file.rs:503",
		);
		expect(
			verifyCitedLiterals(REPO_ROOT, mutated).some((problem) => problem.includes("not readable")),
		).toBe(true);
	});
});