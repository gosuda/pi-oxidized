#!/usr/bin/env bun
/**
 * Terminal copy policy ledger witness suite (TUI-G5, issue #50).
 *
 * Mechanically re-checks every pin in docs/STYLE_LEDGER.md so copy drift fails
 * one command, mirroring G1's acceptance ("zero open items"):
 *
 * 1. verifyAxisSet           - exactly the ten expected axes, no more, none renamed.
 * 2. verifyOneDecisionPerAxis - each axis has exactly one decisive Pinned decision,
 *                               a well-formed witness cell pair, and a ticket.
 * 3. verifyNoPendingMarkers  - neither marked rows nor the whole ledger carry any
 *                              unresolved token (TBD, pending, TODO, MAYBE, ...).
 * 4. verifyCitedLiterals     - every cited `path:line` literal still exists in-tree.
 *
 * Offline-deterministic: reads only the working tree, never the network.
 */

import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

export const LEDGER_PATH = "docs/STYLE_LEDGER.md";

/** The ten enumerated axes from issue #50, in ledger order. */
export const EXPECTED_AXES: readonly { readonly id: number; readonly title: string }[] = [
	{ id: 1, title: "Case" },
	{ id: 2, title: "Noun ledger" },
	{ id: 3, title: "Login forms" },
	{ id: 4, title: "Unknown-error taxonomy" },
	{ id: 5, title: "No-model remediation" },
	{ id: 6, title: "Empty states" },
	{ id: 7, title: "Ellipsis glyph" },
	{ id: 8, title: "Notice template" },
	{ id: 9, title: "CLI stderr register" },
	{ id: 10, title: "Onboarding and consent" },
] as const;

/**
 * Tokens that mark an axis as not-yet-decided. The ledger must be free of every
 * one; the verifier rejects either an empty decision or any of these anywhere.
 * (`placeholder` is deliberately absent: it is a legitimate UI term in the pin.)
 */
export const PENDING_MARKER_RE =
	/\b(TBD|TODO|FIXME|PENDING|UNDECIDED|UNRESOLVED|AMBIGUOUS|CANDIDATE|DECIDE-LATER|MAYBE)\b/i;

/** Any backtick span in a witness cell; the non-site span is the literal. */
const SPAN_RE = /`([^`]+)`/g;

/** A parsed `` path:line `` + literal witness pair. */
export interface WitnessCell {
	readonly path: string;
	readonly line: number;
	readonly literal: string;
}

/** A ledger row split into its six columns (number, axis, decision, ref, rust, ticket). */
export interface StyleLedgerRow {
	readonly id: number;
	readonly axis: string;
	readonly decision: string;
	readonly reference: WitnessCell;
	readonly rust: WitnessCell;
	readonly ticket: string;
}

/**
 * Extract the cells of one markdown table row. A `|` preceded by a backslash is
 * an escaped pipe inside a cell (e.g. the `||` seen in code literals) and is not
 * a column separator; cells are unescaped so the literal matches the source.
 */
export function splitRowCells(line: string): string[] {
	const cells = line.split(/(?<!\\)\|/).map((cell) => cell.trim().replace(/\\\|/g, "|"));
	if (cells[0] === "") cells.shift();
	if (cells[cells.length - 1] === "") cells.pop();
	return cells;
}

/** Parse a witness cell (`path:line` span + one literal span) or undefined. */
export function parseWitnessCell(cell: string): WitnessCell | undefined {
	const spans = [...cell.matchAll(SPAN_RE)].map((match) => match[1] ?? "");
	const site = spans.find((span) => /^[^`]+\.(?:ts|rs):\d+$/.test(span));
	if (site === undefined) return undefined;
	const literals = spans.filter((span) => span !== site);
	if (literals.length !== 1) return undefined;
	const siteMatch = site.match(/^(.+\.(?:ts|rs)):(\d+)$/);
	if (siteMatch === null) return undefined;
	const line = Number(siteMatch[2] ?? 0);
	if (!Number.isSafeInteger(line) || line < 1) return undefined;
	return { path: siteMatch[1] ?? "", line, literal: literals[0] ?? "" };
}

/**
 * Parse every axis `| … |` row out of the ledger. Only six-column rows whose
 * first cell is an axis number are ledger rows; the header, separator, and the
 * two-column downstream-application-map table are skipped. Malformed rows are
 * reported as problems (and skipped) so no non-null assertion is needed and a
 * skipped axis still trips the axis-set count check.
 */
export function parseLedgerRows(ledgerText: string): {
	rows: readonly StyleLedgerRow[];
	problems: readonly string[];
} {
	const problems: string[] = [];
	const rows: StyleLedgerRow[] = [];
	for (const line of ledgerText.split("\n")) {
		if (!line.trim().startsWith("|")) continue;
		const cells = splitRowCells(line);
		if (cells.length !== 6) continue; // downstream map or other table
		if ((cells[0] ?? "") === "#") continue; // column header
		if (cells.every((cell) => /^-{2,}$/.test(cell))) continue; // separator
		const id = Number(cells[0]);
		if (!Number.isInteger(id) || id < 1) {
			problems.push(`axis number "${cells[0] ?? ""}" is not a positive integer: "${line.trim()}"`);
			continue;
		}
		const axis = cells[1] ?? "";
		const reference = parseWitnessCell(cells[3] ?? "");
		const rust = parseWitnessCell(cells[4] ?? "");
		if (reference === undefined) {
			problems.push(`axis ${id} (${axis}) has a malformed reference witness cell`);
			continue;
		}
		if (rust === undefined) {
			problems.push(`axis ${id} (${axis}) has a malformed rust witness cell`);
			continue;
		}
		rows.push({
			id,
			axis,
			decision: cells[2] ?? "",
			reference,
			rust,
			ticket: cells[5] ?? "",
		});
	}
	return { rows, problems };
}

/** Exactly the ten expected axes, each once, in order, none renamed. */
export function verifyAxisSet(ledgerText: string): string[] {
	const { rows, problems } = parseLedgerRows(ledgerText);
	if (problems.length > 0) return [...problems];
	if (rows.length !== EXPECTED_AXES.length) {
		return [`ledger declares ${rows.length} axes, expected ${EXPECTED_AXES.length}`];
	}
	const violations: string[] = [];
	for (let index = 0; index < EXPECTED_AXES.length; index++) {
		const expected = EXPECTED_AXES[index];
		const actual = rows[index];
		if (expected === undefined) continue;
		if (actual === undefined) {
			violations.push(`axis table missing expected axis ${expected.id} (${expected.title})`);
			continue;
		}
		if (actual.id !== expected.id) {
			violations.push(`axis ${index + 1} has id ${actual.id}, expected ${expected.id}`);
		}
		if (actual.axis !== expected.title) {
			violations.push(`axis ${expected.id} is titled "${actual.axis}", expected "${expected.title}"`);
		}
	}
	return violations;
}

/** Every axis has exactly one decisive decision and a non-empty applying ticket. */
export function verifyOneDecisionPerAxis(ledgerText: string): string[] {
	const { rows, problems } = parseLedgerRows(ledgerText);
	if (problems.length > 0) return [...problems];
	const violations: string[] = [];
	for (const row of rows) {
		if (row.decision.trim() === "") {
			violations.push(`axis ${row.id} (${row.axis}) has an empty Pinned decision`);
		} else if (PENDING_MARKER_RE.test(row.decision)) {
			violations.push(`axis ${row.id} (${row.axis}) Pinned decision carries an unresolved marker`);
		}
		if (row.ticket.trim() === "") {
			violations.push(`axis ${row.id} (${row.axis}) names no applying ticket`);
		}
	}
	return violations;
}

/** No unresolved token anywhere in the ledger body. */
export function verifyNoPendingMarkers(ledgerText: string): string[] {
	const lines = ledgerText.split("\n");
	const violations: string[] = [];
	for (let index = 0; index < lines.length; index++) {
		const line = lines[index] ?? "";
		if (PENDING_MARKER_RE.test(line)) {
			violations.push(`line ${index + 1} carries an unresolved marker: "${line.trim()}"`);
		}
	}
	return violations;
}

/** Every cited `path:line` literal must still exist in the file sitting on disk. */
export function verifyCitedLiterals(root: string, ledgerText: string): string[] {
	const { rows, problems } = parseLedgerRows(ledgerText);
	if (problems.length > 0) return [...problems];
	const violations: string[] = [];
	for (const row of rows) {
		for (const cell of [["reference", row.reference], ["rust", row.rust]] as const) {
			try {
				const content = readFileSync(join(root, cell[1].path), "utf8");
				const citedLine = content.split(/\r?\n/u)[cell[1].line - 1];
				if (citedLine === undefined) {
					violations.push(
						`axis ${row.id} ${cell[0]} line ${cell[1].line} is outside ${cell[1].path}`,
					);
				} else if (!citedLine.includes(cell[1].literal)) {
					violations.push(
						`axis ${row.id} ${cell[0]} literal ${JSON.stringify(cell[1].literal)} is not at ${cell[1].path}:${cell[1].line}`,
					);
				}
			} catch {
				violations.push(`axis ${row.id} ${cell[0]} cited path ${cell[1].path} is not readable`);
			}
		}
	}
	return violations;
}

/** Run every style-ledger witness against one repository root; empty means green. */
export function runStyleLedgerWitnesses(root: string): string[] {
	const violations: string[] = [];
	const add = (witness: string, results: readonly string[]): void => {
		for (const result of results) violations.push(`[${witness}] ${result}`);
	};

	let ledgerText: string | null = null;
	try {
		ledgerText = readFileSync(join(root, LEDGER_PATH), "utf8");
	} catch {
		add("ledger", [`${LEDGER_PATH} is not readable`]);
	}

	if (ledgerText !== null) {
		add("axis-set", verifyAxisSet(ledgerText));
		add("one-decision", verifyOneDecisionPerAxis(ledgerText));
		add("no-pending", verifyNoPendingMarkers(ledgerText));
		add("cited-literals", verifyCitedLiterals(root, ledgerText));
	}

	return violations;
}

function main(): void {
	const violations = runStyleLedgerWitnesses(REPO_ROOT);
	if (violations.length > 0) {
		console.error(`style-ledger witness suite failed with ${violations.length} violation(s):`);
		for (const violation of violations) console.error(`  - ${violation}`);
		process.exit(1);
	}
	process.stdout.write("STYLE_LEDGER_WITNESSES_OK\n");
}

if (import.meta.main) main();