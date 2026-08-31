import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
	EXPECTED_EXTERNAL_COUNT,
	EXPECTED_ROW_COUNT,
	EXPECTED_SOURCE_RECORD_COUNT,
	EXPECTED_TASK_COUNT,
	EXECUTION_MAP_CURRENT_PATH,
	EXECUTION_MAP_DIRECTORY,
	EXECUTION_MAP_GENERATIONS_DIRECTORY,
	MAP_ROOT_ID,
	PARITY_LEDGER_PATH,
	TRACK_CLOSERS,
	SNAPSHOT_SOURCE_HASH,
	SNAPSHOT_STRUCTURAL_SHA256,
	TERMINAL_NODE_ID,
	computeExecutionMapGenerationId,
	computeSnapshotSourceHash,
	computeSnapshotStructuralHash,
	deriveExpectedRegistry,
	extractExecutionMapBundle,
	isExecutionMapGenerationPath,
	loadCurrentExecutionMap,
	loadMapLedgerInputs,
	parseExecutionMap,
	parseExecutionMapPointer,
	parseSnapshot,
	renderExecutionMapPointer,
	runMapLedgerChecks,
} from "./map.ts";
import { section as extractSection } from "./fetch-map-source.ts";
import { PINNED_AGENT_LOOP_CONFIG_SITES, REPO_ROOT } from "./parity.ts";

const CURRENT_EXECUTION_MAP = loadCurrentExecutionMap(REPO_ROOT);
const SNAPSHOT_TEXT = CURRENT_EXECUTION_MAP.witnessText;
const LEDGER_TEXT = readFileSync(join(REPO_ROOT, PARITY_LEDGER_PATH), "utf8");
const MAP_TEXT = CURRENT_EXECUTION_MAP.mapText;

function run(mapText: string, snapshotText: string = SNAPSHOT_TEXT): string[] {
	return runMapLedgerChecks({ snapshotText, ledgerText: LEDGER_TEXT, mapText });
}

type CellEdit = (cells: string[]) => string[];

function editRow(mapText: string, id: string, edit: CellEdit): string {
	const lines = mapText.split("\n");
	const prefix = `| ${id} |`;
	const index = lines.findIndex((line) => line.startsWith(prefix));
	if (index < 0) throw new Error(`row ${id} not found`);
	const line = lines[index];
	if (line === undefined) throw new Error(`row ${id} not found`);
	const cells = line.slice(2, -2).split(" | ");
	lines[index] = `| ${edit(cells).join(" | ")} |`;
	return lines.join("\n");
}

function rowLine(mapText: string, id: string): string {
	const line = mapText.split("\n").find((entry) => entry.startsWith(`| ${id} |`));
	if (line === undefined) throw new Error(`row ${id} not found`);
	return line;
}

function dropBlocker(id: string, blocker: string): CellEdit {
	return (cells) => {
		const blockedBy = cells[4];
		if (blockedBy === undefined || !blockedBy.includes(blocker)) {
			throw new Error(`row ${id} does not list ${blocker}`);
		}
		cells[4] = blockedBy
			.split(", ")
			.filter((entry) => entry !== blocker)
			.join(", ");
		return cells;
	};
}

function addBlocker(extra: string): CellEdit {
	return (cells) => {
		const blockedBy = cells[4];
		if (blockedBy === undefined) throw new Error("row has no blocked_by cell");
		cells[4] = blockedBy === "—" ? extra : `${blockedBy}, ${extra}`;
		return cells;
	};
}

function deleteRow(mapText: string, id: string): string {
	const lines = mapText.split("\n");
	const prefix = `| ${id} |`;
	const index = lines.findIndex((line) => line.startsWith(prefix));
	if (index < 0) throw new Error(`row ${id} not found`);
	lines.splice(index, 1);
	return lines.join("\n");
}

function appendRow(mapText: string, row: string): string {
	return mapText.replace(
		"\n## Pinned telemetry migration surface",
		`\n${row}\n\n## Pinned telemetry migration surface`,
	);
}

describe("execution map ledger (MAP-1)", () => {
	test("baseline: the published snapshot, ledger, and map stay green together", () => {
		expect(run(MAP_TEXT)).toEqual([]);
	});

	test("row count is exactly 151: 137 siblings + 6 map tickets + 7 externals + MAP-ROOT", () => {
		const doc = parseExecutionMap(MAP_TEXT);
		expect(doc.rows.length).toBe(EXPECTED_ROW_COUNT);
		const parsed = parseSnapshot(SNAPSHOT_TEXT);
		if (parsed.snapshot === null) throw new Error("snapshot must parse");
		const derived = deriveExpectedRegistry(parsed.snapshot);
		expect(derived.problems).toEqual([]);
		expect(derived.rows.length).toBe(EXPECTED_ROW_COUNT);
		expect(derived.rows.filter((row) => row.recordKind === "execution").length).toBe(143);
		expect(
			derived.rows.filter((row) => row.recordKind === "external").map((row) => row.stableId),
		).toEqual(["EXT-14", "EXT-15", "EXT-21", "EXT-23", "EXT-24", "EXT-25", "EXT-26"]);
		expect(derived.rows.some((row) => row.stableId === MAP_ROOT_ID)).toBe(true);
		for (const closer of TRACK_CLOSERS) {
			expect(doc.rows.some((row) => row.stableId === closer)).toBe(true);
		}
		expect(doc.telemetrySites.length).toBe(PINNED_AGENT_LOOP_CONFIG_SITES.length);
		expect(doc.headerHash).toBe(SNAPSHOT_STRUCTURAL_SHA256);
		expect(computeSnapshotStructuralHash(parsed.snapshot)).toBe(SNAPSHOT_STRUCTURAL_SHA256);
		expect(parsed.snapshot.sourceHash).toBe(SNAPSHOT_SOURCE_HASH);
		expect(computeSnapshotSourceHash(parsed.snapshot.records)).toBe(SNAPSHOT_SOURCE_HASH);
		expect(parsed.snapshot.sourceRecordCount).toBe(EXPECTED_SOURCE_RECORD_COUNT);
		expect(parsed.snapshot.records.length).toBe(EXPECTED_SOURCE_RECORD_COUNT);
		expect(parsed.snapshot.taskCount).toBe(EXPECTED_TASK_COUNT);
		expect(derived.rows.filter((row) => row.recordKind === "execution").length).toBe(EXPECTED_TASK_COUNT);
		expect(parsed.snapshot.externalCount).toBe(EXPECTED_EXTERNAL_COUNT);
		expect(parsed.snapshot.records.filter((record) => record.kind === "external").length).toBe(
			EXPECTED_EXTERNAL_COUNT,
		);
	});

	test("mutation: dropping REL-CLOSE <- REL-DOCS fails the required closure edge", () => {
		const mutated = editRow(MAP_TEXT, "REL-CLOSE", dropBlocker("REL-CLOSE", "REL-DOCS"));
		const violations = run(mutated);
		expect(violations.some((entry) => entry.includes("required closure edge REL-CLOSE <- REL-DOCS"))).toBe(
			true,
		);
	});

	test("mutation: renaming a REL-DOCS reference strands a missing node and a banned spelling", () => {
		const mutated = editRow(MAP_TEXT, "DOC-F", (cells) => {
			const blockedBy = cells[4];
			if (blockedBy === undefined || !blockedBy.includes("REL-DOCS")) {
				throw new Error("DOC-F does not list REL-DOCS");
			}
			cells[4] = blockedBy.replace("REL-DOCS", "REL-PKGDOC");
			return cells;
		});
		const violations = run(mutated);
		expect(
			violations.some((entry) => entry.includes("[resolution] DOC-F blocked_by references missing node REL-PKGDOC")),
		).toBe(true);
		expect(violations.some((entry) => entry.includes("banned retired spelling 'REL-PKGDOC'"))).toBe(true);
	});

	test("mutation: a second-name row for one issue fails alias detection", () => {
		const mutated = appendRow(
			MAP_TEXT,
			"| REL-PKGDOC | task | #111 | Second retired name for the release documentation staging row | REL-T4, REL-T6 |",
		);
		const violations = run(mutated);
		expect(violations.some((entry) => entry.includes("two rows share issue #111"))).toBe(true);
		expect(violations.some((entry) => entry.includes("banned retired spelling 'REL-PKGDOC'"))).toBe(true);
		expect(
			violations.some(
				(entry) => entry.includes("row REL-PKGDOC") && entry.includes("matches no witness record"),
			),
		).toBe(true);
	});

	test("mutation: an exact duplicate row fails duplicate detection", () => {
		const mutated = appendRow(MAP_TEXT, rowLine(MAP_TEXT, "REL-DOCS"));
		const violations = run(mutated);
		expect(violations.some((entry) => entry.includes("[duplicate] stable ID REL-DOCS appears in 2 rows"))).toBe(
			true,
		);
	});

	test("mutation: dropping DOC-F <- DEPS-D1 fails the required prerequisite edge", () => {
		const mutated = editRow(MAP_TEXT, "DOC-F", dropBlocker("DOC-F", "DEPS-D1"));
		const violations = run(mutated);
		expect(violations.some((entry) => entry.includes("required prerequisite DEPS-D1 of DOC-F is missing"))).toBe(
			true,
		);
	});

	test("mutation: adding PAR-LEDGER <- PAR-CLOSE fails the acyclicity check", () => {
		const mutated = editRow(MAP_TEXT, "PAR-LEDGER", addBlocker("PAR-CLOSE"));
		const violations = run(mutated);
		const cyclic = violations.filter((entry) => entry.startsWith("[acyclicity]"));
		expect(cyclic.length).toBe(1);
		expect(cyclic[0]).toContain("PAR-LEDGER");
	});

	test("mutation: a retired S-suffixed row spelling fails the alias scan", () => {
		const mutated = editRow(MAP_TEXT, "DOC-A", (cells) => {
			cells[0] = "DOCS-A";
			return cells;
		});
		const violations = run(mutated);
		expect(violations.some((entry) => entry.includes("banned retired spelling 'DOCS-A'"))).toBe(true);
		expect(violations.some((entry) => entry.includes("registry row DOC-A is missing"))).toBe(true);
	});

	test("mutation: emptying a graduation modality fails the population check", () => {
		const mutated = MAP_TEXT.replaceAll("| research |", "| task |");
		const violations = run(mutated);
		expect(violations.some((entry) => entry.includes("graduation modality 'research' has no rows"))).toBe(true);
	});

	test("mutation: deleting a row fails the row set and strands its dependents", () => {
		const mutated = deleteRow(MAP_TEXT, "TUI-P3");
		const violations = run(mutated);
		expect(violations.some((entry) => entry.includes("registry row TUI-P3 is missing"))).toBe(true);
		expect(
			violations.some((entry) => entry.includes("[resolution] TUI-CLOSE blocked_by references missing node TUI-P3")),
		).toBe(true);
	});

	test("mutation: breaking MAP-5 closure composition fails the closure check", () => {
		const mutated = editRow(MAP_TEXT, "MAP-5", dropBlocker("MAP-5", "PAR-CLOSE"));
		const violations = run(mutated);
		expect(violations.some((entry) => entry.includes("MAP-5 closure composition is missing PAR-CLOSE"))).toBe(
			true,
		);
	});

	test("mutation: telemetry pin drift fails the documented-site check", () => {
		const mutated = MAP_TEXT.replace(
			"crates/pi-agent/src/agent.rs:62-88",
			"crates/pi-agent/src/agent.rs:62-89",
		);
		const violations = run(mutated);
		expect(
			violations.some((entry) =>
				entry.includes("[telemetry] documented site crates/pi-agent/src/agent.rs:62-89 matches no pinned"),
			),
		).toBe(true);
	});

	test("mutation: a tampered witness fails the mapped structural hash pin", () => {
		const parsed: unknown = JSON.parse(SNAPSHOT_TEXT);
		if (typeof parsed !== "object" || parsed === null) throw new Error("snapshot must parse");
		const container = parsed as { records?: unknown[] };
		for (const record of container.records ?? []) {
			if (
				typeof record === "object" &&
				record !== null &&
				(record as { stableId?: unknown }).stableId === "REL-DOCS"
			) {
				(record as { title?: unknown }).title = "Hand-edited title";
			}
		}
		const violations = run(MAP_TEXT, JSON.stringify(parsed));
		expect(
			violations.some((entry) => entry.includes("recomputed snapshot structural sha256") && entry.includes("does not match the pinned")),
		).toBe(true);
	});

	test("mutation: editing a source-only record field fails the source hash, not the mapped hash", () => {
		const parsed: unknown = JSON.parse(SNAPSHOT_TEXT);
		if (typeof parsed !== "object" || parsed === null) throw new Error("snapshot must parse");
		const container = parsed as { records?: unknown[] };
		for (const record of container.records ?? []) {
			if (typeof record !== "object" || record === null || !("stableId" in record)) continue;
			if (record.stableId !== "REL-DOCS") continue;
			// stableId checked above; cast only for the sibling field assignment.
			const witnessRecord = record as Record<string, unknown>;
			witnessRecord.question = "Hand-edited question";
		}
		const witnessText = JSON.stringify(parsed);
		const parsedWitness = parseSnapshot(witnessText);
		if (parsedWitness.snapshot === null) throw new Error("mutated witness must still parse");
		// The question field is absent from the mapped structural hash input:
		// a source-only edit must leave it untouched.
		expect(computeSnapshotStructuralHash(parsedWitness.snapshot)).toBe(SNAPSHOT_STRUCTURAL_SHA256);
		const violations = run(MAP_TEXT, witnessText);
		expect(
			violations.some((entry) =>
				entry.startsWith("[source-hash]") && entry.includes("recomputed source-record hash"),
			),
		).toBe(true);
		expect(violations.filter((entry) => entry.startsWith("[structural-hash]"))).toEqual([]);
	});

	test("mutation: a non-canonical source field reports a stable source-hash violation", () => {
		const parsed: unknown = JSON.parse(SNAPSHOT_TEXT);
		if (typeof parsed !== "object" || parsed === null) throw new Error("snapshot must parse");
		const records = (parsed as { records?: unknown[] }).records;
		if (records === undefined || typeof records[0] !== "object" || records[0] === null) {
			throw new Error("snapshot must contain a record");
		}
		(records[0] as Record<string, unknown>).noncanonical = 1.5;

		const violations = run(MAP_TEXT, JSON.stringify(parsed));
		expect(
			violations.some(
				(entry) =>
					entry.startsWith("[source-hash]") &&
					entry.includes("cannot canonicalize structural ticket records"),
			),
		).toBe(true);
	});

	test("mutation: a release bypass edge fails the delete-node dominance simulation", () => {
		const mutated = editRow(MAP_TEXT, "DOC-F", addBlocker("REL-T9"));
		const violations = run(mutated);
		expect(
			violations.some((entry) =>
				entry.includes("release node REL-T9 reaches DOC-F around the REL-DOCS/REL-CLOSE gate"),
			),
		).toBe(true);
	});

	test("mutation: deleting the doc header hash line fails the structural pin", () => {
		const headerLine = MAP_TEXT.split("\n").find((line) => line.includes("Snapshot structural sha256:"));
		if (headerLine === undefined) throw new Error("map document must state the snapshot structural hash");
		const mutated = MAP_TEXT.replace(headerLine, "");
		const violations = run(mutated);
		expect(
			violations.some((entry) => entry.startsWith("[structural-hash]") && entry.includes("must state exactly")),
		).toBe(true);
	});

	test("mutation: a dangling external reference fails resolution", () => {
		const mutated = editRow(MAP_TEXT, "DEPS-G1", addBlocker("EXT-13"));
		const violations = run(mutated);
		expect(
			violations.some((entry) => entry.includes("[resolution] DEPS-G1 blocked_by references missing node EXT-13")),
		).toBe(true);
	});

	test("loader: a checkout missing the current pointer fails with a stable input diagnostic", () => {
		const missingRoot = join(REPO_ROOT, "scripts", "verification", "no-such-checkout-root");
		const load = () => loadMapLedgerInputs(missingRoot);
		expect(load).toThrow("cannot read required");
		expect(load).toThrow(EXECUTION_MAP_CURRENT_PATH);
	});

	test("mutation: a document-only zero-blocker island fails two-sided MAP-ROOT anchoring", () => {
		let island = MAP_TEXT;
		island = appendRow(island, "| ISLE-B | task | #999 | Island dependent that never reaches the terminal | ISLE-A |");
		island = appendRow(island, "| ISLE-A | task | #998 | Island root outside the canonical frontier | — |");
		const violations = run(island);
		const anchoring = violations.filter((entry) => entry.startsWith("[anchoring]"));
		for (const id of ["ISLE-A", "ISLE-B"]) {
			const row = anchoring.find((entry) => entry.includes(`orphan row ${id}`));
			if (row === undefined) throw new Error(`${id} must be reported as an anchoring orphan`);
			expect(row).toContain(`not reachable from ${MAP_ROOT_ID}`);
			expect(row).toContain(`no path into terminal ${TERMINAL_NODE_ID}`);
		}
	});

	test("mutation: dropping ARC-CLOSE from MAP-5 fails the closure composition check", () => {
		const mutated = editRow(MAP_TEXT, "MAP-5", dropBlocker("MAP-5", "ARC-CLOSE"));
		const violations = run(mutated);
		expect(violations.some((entry) => entry.includes("MAP-5 closure composition is missing ARC-CLOSE"))).toBe(
			true,
		);
	});

	test("mutation: a document modality that differs from the witness fails row-field check", () => {
		const mutated = editRow(MAP_TEXT, "ARC-CLOSE", (cells) => {
			cells[1] = "research";
			return cells;
		});
		const violations = run(mutated);
		expect(
			violations.some((entry) => entry.includes("ARC-CLOSE modality 'research' differs from witness 'task'")),
		).toBe(true);
	});

	test("section parser: multi-bullet Blocked by is not truncated", () => {
		const body = [
			"Stable ID: `ARC-T2`",
			"",
			"## Question",
			"",
			"Question text",
			"",
			"## Blocked by",
			"",
			"- [ARC-R1](https://github.com/gosuda/pi-oxidized/issues/153)",
			"- [ARC-T1](https://github.com/gosuda/pi-oxidized/issues/158)",
			"- [ARC-R2](https://github.com/gosuda/pi-oxidized/issues/154)",
			"",
			"## Acceptance",
			"",
			"Acceptance text",
		].join("\n");
		const blockedBy = extractSection(body, "Blocked by", true);
		expect(blockedBy).toContain("ARC-R1");
		expect(blockedBy).toContain("ARC-T1");
		expect(blockedBy).toContain("ARC-R2");
		const question = extractSection(body, "Question", true);
		expect(question).toBe("Question text");
		const acceptance = extractSection(body, "Acceptance", true);
		expect(acceptance).toBe("Acceptance text");
	});

	test("mutation: stale count pins fail the source-hash check", () => {
		const parsed: unknown = JSON.parse(SNAPSHOT_TEXT);
		if (typeof parsed !== "object" || parsed === null) throw new Error("snapshot must parse");
		const container = parsed as { sourceRecordCount?: unknown; records?: unknown[] };
		container.sourceRecordCount = (container.sourceRecordCount as number) + 1;
		const violations = run(MAP_TEXT, JSON.stringify(parsed));
		expect(
			violations.some((entry) =>
				entry.includes("[source-hash]") && entry.includes("sourceRecordCount"),
			),
		).toBe(true);
	});

	test("section parser: suffixed required heading is rejected while trailing whitespace and CRLF remain accepted", () => {
		const body = [
			"Stable ID: `ARC-T2`",
			"",
			"## Question draft",
			"",
			"Should not match",
			"",
			"## Blocked by TODO",
			"",
			"Should not match either",
			"",
			"## Question",
			"",
			"Real question text",
			"",
			"## Acceptance",
			"",
			"Acceptance text",
		].join("\n");
		const question = extractSection(body, "Question", true);
		expect(question).toBe("Real question text");
		expect(question).not.toContain("Should not match");
		const crlfBody = "Stable ID: `ARC-T2`\r\n\r\n## Question \r\n\r\nCRLF question\r\n\r\n## Acceptance\r\n\r\nAcceptance\r\n";
		const crlfQuestion = extractSection(crlfBody, "Question", true);
		expect(crlfQuestion).toBe("CRLF question");
		const trailingSpaceBody = "## Question   \n\nTrailing space question\n\n## Acceptance\n\nAcceptance\n";
		const trailingSpaceQuestion = extractSection(trailingSpaceBody, "Question", true);
		expect(trailingSpaceQuestion).toBe("Trailing space question");
		let suffixedThrew = false;
		try {
			extractSection("## Question draft\n\nbody\n", "Question", true);
		} catch {
			suffixedThrew = true;
		}
		expect(suffixedThrew).toBe(true);
	});
});

describe("execution-map immutable publication reader", () => {
	const READER_MAP_TEXT = "# Execution map\n\nRendered map body for the reader fixture.\n";
	const READER_WITNESS_JSON = '{\n  "version": 2\n}\n';

	function bundleOf(mapText: string, witnessJson: string): string {
		return `${mapText}\n## Canonical witness\n\n\`\`\`json\n${witnessJson}\`\`\`\n`;
	}

	function stagePublication(root: string, mapText: string, witnessJson: string): string {
		const generationText = bundleOf(mapText, witnessJson);
		const generationId = computeExecutionMapGenerationId(generationText);
		mkdirSync(join(root, EXECUTION_MAP_GENERATIONS_DIRECTORY), { recursive: true });
		writeFileSync(join(root, EXECUTION_MAP_CURRENT_PATH), renderExecutionMapPointer(generationId));
		writeFileSync(join(root, EXECUTION_MAP_GENERATIONS_DIRECTORY, `${generationId}.md`), generationText);
		return generationId;
	}

	test("pointer render and parse round-trip the exact grammar", () => {
		const generationId = "0a".repeat(32);
		const pointer = renderExecutionMapPointer(generationId);
		expect(pointer).toBe(`[Current execution map](generations/${generationId}.md)\n`);
		expect(parseExecutionMapPointer(pointer)).toBe(generationId);
	});

	test("malformed pointers fail closed", () => {
		const generationId = "0a".repeat(32);
		const pointer = renderExecutionMapPointer(generationId);
		const malformed = [
			pointer.trimEnd(),
			`${pointer}${pointer}`,
			pointer.replace(generationId, generationId.toUpperCase()),
			`[Execution map](generations/${generationId}.md)\n`,
			`[Current execution map](legacy/map.md)\n`,
			`[Current execution map](generations/${"z".repeat(64)}.md)\n`,
			`[Current execution map](generations/${generationId.slice(0, 63)}.md)\n`,
		];
		for (const candidate of malformed) {
			expect(() => parseExecutionMapPointer(candidate)).toThrow("malformed execution-map pointer");
		}
	});

	test("generation path recognition accepts only content-addressed generations", () => {
		expect(isExecutionMapGenerationPath(`generations/${"0a".repeat(32)}.md`)).toBe(true);
		const rejected = [
			"generations/current.md",
			"generations/0a.md",
			`generations/${"0A".repeat(32)}.md`,
			`generations/${"0a".repeat(31)}.md`,
			".staging/0a.md",
			"legacy-map.md",
		];
		for (const candidate of rejected) {
			expect(isExecutionMapGenerationPath(candidate)).toBe(false);
		}
	});

	test("bundle extraction restores exact map bytes and witness body", () => {
		const bundle = extractExecutionMapBundle(bundleOf(READER_MAP_TEXT, READER_WITNESS_JSON));
		expect(bundle.mapText).toBe(READER_MAP_TEXT);
		expect(bundle.witnessText).toBe(READER_WITNESS_JSON);
		expect(bundle.bundleText).toBe(bundleOf(READER_MAP_TEXT, READER_WITNESS_JSON));
	});

	test("bundle extraction rejects non-terminal witness grammar", () => {
		const doubleWitnessMap = `${READER_MAP_TEXT}\n## Canonical witness\n\n\`\`\`json\n{}\n\`\`\`\n`;
		const malformedBundles = [
			READER_MAP_TEXT,
			bundleOf(doubleWitnessMap, READER_WITNESS_JSON),
			bundleOf(READER_MAP_TEXT, "{}"),
			bundleOf(READER_MAP_TEXT, "not json\n"),
			`${bundleOf(READER_MAP_TEXT, READER_WITNESS_JSON)}trailing bytes\n`,
		];
		for (const candidate of malformedBundles) {
			expect(() => extractExecutionMapBundle(candidate)).toThrow("malformed execution-map generation");
		}
	});

	test("loadCurrentExecutionMap returns the pointer-selected generation", () => {
		const root = mkdtempSync(join(tmpdir(), "execution-map-reader-ok-"));
		const generationId = stagePublication(root, READER_MAP_TEXT, READER_WITNESS_JSON);
		const current = loadCurrentExecutionMap(root);
		expect(current.generationId).toBe(generationId);
		expect(current.mapText).toBe(READER_MAP_TEXT);
		expect(current.witnessText).toBe(READER_WITNESS_JSON);
		expect(current.bundleText).toBe(bundleOf(READER_MAP_TEXT, READER_WITNESS_JSON));
		rmSync(root, { recursive: true, force: true });
	});

	test("loadCurrentExecutionMap rejects a malformed pointer", () => {
		const root = mkdtempSync(join(tmpdir(), "execution-map-reader-pointer-"));
		mkdirSync(join(root, EXECUTION_MAP_GENERATIONS_DIRECTORY), { recursive: true });
		writeFileSync(join(root, EXECUTION_MAP_CURRENT_PATH), "[Current execution map](generations/deadbeef.md)\n");
		expect(() => loadCurrentExecutionMap(root)).toThrow("malformed execution-map pointer");
		rmSync(root, { recursive: true, force: true });
	});

	test("loadCurrentExecutionMap rejects a missing generation", () => {
		const root = mkdtempSync(join(tmpdir(), "execution-map-reader-missing-"));
		mkdirSync(join(root, EXECUTION_MAP_DIRECTORY), { recursive: true });
		writeFileSync(join(root, EXECUTION_MAP_CURRENT_PATH), renderExecutionMapPointer("1b".repeat(32)));
		const load = () => loadCurrentExecutionMap(root);
		expect(load).toThrow("cannot read required");
		expect(load).toThrow(`generations/${"1b".repeat(32)}.md`);
		rmSync(root, { recursive: true, force: true });
	});

	test("loadCurrentExecutionMap rejects a digest mismatch", () => {
		const root = mkdtempSync(join(tmpdir(), "execution-map-reader-digest-"));
		const generationId = stagePublication(root, READER_MAP_TEXT, READER_WITNESS_JSON);
		writeFileSync(join(root, EXECUTION_MAP_GENERATIONS_DIRECTORY, `${generationId}.md`), `${READER_MAP_TEXT}tampered\n`);
		expect(() => loadCurrentExecutionMap(root)).toThrow("digest mismatch");
		rmSync(root, { recursive: true, force: true });
	});

	test("loadMapLedgerInputs serves both inputs from the pointer-selected generation", () => {
		const root = mkdtempSync(join(tmpdir(), "execution-map-ledger-"));
		mkdirSync(join(root, "docs"), { recursive: true });
		writeFileSync(join(root, PARITY_LEDGER_PATH), LEDGER_TEXT);
		stagePublication(root, READER_MAP_TEXT, READER_WITNESS_JSON);
		const inputs = loadMapLedgerInputs(root);
		expect(inputs.snapshotText).toBe(READER_WITNESS_JSON);
		expect(inputs.ledgerText).toBe(LEDGER_TEXT);
		expect(inputs.mapText).toBe(READER_MAP_TEXT);
		rmSync(root, { recursive: true, force: true });
	});
});
