#!/usr/bin/env bun
/**
 * Execution-map ledger verifier (MAP-1, issue #134).
 *
 * docs/EXECUTION_MAP.md is the published view of the port program's
 * stable-ID DAG. The authority is the live GitHub issue tree rooted at
 * canonical issue #12; the tracked fixture at
 * scripts/verification/fixtures/execution-map-ticket-records.json is the
 * commit-pinned offline witness published from that authority — not a
 * live API view. This tool never reads a shadow graph: it re-derives the
 * expected row set from the witness and recomputes both its source-record
 * provenance hash and the mapped structural sha256, so hand edits to any
 * side fail one command. `bun run verify:map-ledger` is the single
 * acceptance path and prints MAP_LEDGER_OK when every assertion holds:
 *
 * 1. witness pins           - v2 witness, repository, canonical issue #12,
 *                             declared source hash matches its pin and the
 *                             recomputed publisher-canonical hash;
 *                             131 records / 115 tasks / 16 externals
 *                             agree; mapped structural sha256 matches the
 *                             pinned value. The two hashes guard distinct
 *                             contracts: full ticket-record provenance
 *                             versus the mapped registry rows.
 * 2. row set                - exactly the 123 expected rows, no extras, none
 *                             missing, no duplicate stable IDs, no two rows
 *                             sharing one issue (alias detection).
 * 3. row fields             - every row's Issue, Title, and blocked_by match
 *                             the witness record exactly (ordered blockers).
 * 4. resolution             - every blocked_by reference resolves to a row.
 * 5. acyclicity             - Kahn topological sort over blocked_by edges.
 * 6. anchoring              - the verifier alone adds synthetic
 *                             MAP-ROOT -> frontier edges (fixture-derived
 *                             zero-blocker rows only); every published row
 *                             must be reachable from MAP-ROOT alone and
 *                             must reach terminal MAP-6. Published
 *                             blocked_by cells stay synthetic-free.
 * 7. modalities             - vocabulary pinned to PARITY_LEDGER's settled
 *                             kinds, PAR rows matching the ledger's
 *                             graduated DAG, all four graduation modalities
 *                             populated.
 * 8. closure composition    - REL-CLOSE and DOC-F required closure edges,
 *                             MAP-5 composition over the seven closers,
 *                             MAP-6 blocked by exactly MAP-5.
 * 9. REL-DOCS dominance     - no REL-* node reaches DOC-F around the
 *                             REL-DOCS/REL-CLOSE gate (delete-node
 *                             simulation, with single-deletion non-vacuity).
 * 10. telemetry pin         - the documented five AgentLoopConfig sites are
 *                             exactly parity.ts's pinned oracle.
 *
 * Each check is a pure function over text so the mutation suite can drive
 * malformed, aliased, cyclic, and tampered fixtures through them. Input
 * loading is the only impure boundary (loadMapLedgerInputs).
 */


import { readFileSync } from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { PINNED_AGENT_LOOP_CONFIG_SITES, REPO_ROOT } from "./parity.ts";

// ============================================================================
// Published pins
// ============================================================================

export const SNAPSHOT_PATH = "scripts/verification/fixtures/execution-map-ticket-records.json";
export const MAP_DOC_PATH = "docs/EXECUTION_MAP.md";
export const PARITY_LEDGER_PATH = "docs/PARITY_LEDGER.md";

export const MAP_ROOT_ID = "MAP-ROOT";
export const MAP_ROOT_TITLE = "Port program root; anchors the zero-prerequisite frontier";
export const TERMINAL_NODE_ID = "MAP-6";
export const CLOSURE_GATE_NODE_ID = "MAP-5";
export const REL_DOCS_ID = "REL-DOCS";
export const REL_CLOSE_ID = "REL-CLOSE";
export const DOC_F_ID = "DOC-F";

/** The seven track closers, in canonical order. */
export const SEVEN_CLOSERS: readonly string[] = [
	"PAR-CLOSE",
	"XC-CLOSE",
	"TUI-CLOSE",
	"PERF-CLOSE",
	"REL-CLOSE",
	"DEPS-D1",
	"DOC-F",
];

/** 123 rows: 109 sibling graduate tickets + 6 map tickets + 7 externals + MAP-ROOT. */
export const EXPECTED_ROW_COUNT = 123;

/** Witness provenance counts pinned from the publishing authority. */
export const EXPECTED_SOURCE_RECORD_COUNT = 131;
export const EXPECTED_TASK_COUNT = 115;
export const EXPECTED_EXTERNAL_COUNT = 16;
/**
 * Retired spellings that must appear nowhere in the registry or published
 * fields. MAP-1's own snapshot record is exempt because its acceptance text
 * enumerates them.
 */
export const BANNED_ALIAS_TOKENS: readonly string[] = ["EXT-PARITY", "REL-PKGDOC", "REL-T10", "REL-G1"];
/** Any S-suffixed retired DOCS-* spelling (the settled rows are DOC-*). */
export const BANNED_ALIAS_PATTERN = /DOCS-[A-Z0-9]+/g;
export const BANNED_PHRASE = "six struct-literal";

/** Structural sha256 over the mapped snapshot records; rejects hand edits. */
export const SNAPSHOT_STRUCTURAL_SHA256 = "abc3c1b4f09b145e7ae9cdf3510d5dd9c21022dce1ff565444888f9f360a88ad";

/**
 * Pinned canonical source-record hash of the tracked witness: the
 * publisher's SHA-256 over canonical UTF-8 JSON (sorted object keys,
 * compact separators, record order preserved) of all 131 structural ticket
 * records, with mutable issue status excluded. Guards full ticket-record
 * provenance — a distinct contract from the mapped structural hash above,
 * which guards the derived registry rows.
 */
export const SNAPSHOT_SOURCE_HASH = "sha256:8ab7e57344727c59a722359adb852c6a8e4d98c53fee4df48fe103ffaf5aadfd";

// ============================================================================
// Snapshot ingestion
// ============================================================================

export interface SnapshotRecord {
	/** Structural ticket record: `execution` task or `external` prerequisite. */
	readonly kind: "execution" | "external";
	readonly stableId: string;
	readonly issue: number;
	readonly url: string;
	readonly title: string;
	readonly question: string | null;
	readonly acceptance: string | null;
	readonly nativeParent: string;
	readonly blockers: readonly string[];
}

export interface Snapshot {
	readonly version: number;
	readonly repository: string;
	readonly canonicalIssue: number;
	readonly sourceHash: string;
	readonly sourceRecordCount: number;
	readonly taskCount: number;
	readonly externalCount: number;
	readonly records: readonly SnapshotRecord[];
}

export interface ExpectedRow {
	readonly stableId: string;
	/** `execution`, `external`, or `map-root` for the synthetic anchor. */
	readonly recordKind: string;
	readonly issue: number;
	readonly title: string;
	readonly blockedBy: readonly string[];
	readonly record: SnapshotRecord | null;
}

export interface DerivedRegistry {
	readonly rows: readonly ExpectedRow[];
	readonly problems: readonly string[];
}

const RECORD_KINDS: Record<string, true> = { execution: true, external: true };

/**
 * Validate every retained structural field: kind, nonempty stable ID,
 * integer issue, URL, nullable question/acceptance, native parent, and
 * ordered string blockers. Mutable issue status is not part of the
 * tracked-witness contract.
 */
function isSnapshotRecord(value: unknown): value is SnapshotRecord {
	if (typeof value !== "object" || value === null) return false;
	const candidate = value as Partial<SnapshotRecord>;
	return (
		typeof candidate.stableId === "string" &&
		candidate.stableId.length > 0 &&
		typeof candidate.kind === "string" &&
		RECORD_KINDS[candidate.kind] === true &&
		typeof candidate.issue === "number" &&
		Number.isInteger(candidate.issue) &&
		typeof candidate.url === "string" &&
		typeof candidate.title === "string" &&
		(candidate.question === null || typeof candidate.question === "string") &&
		(candidate.acceptance === null || typeof candidate.acceptance === "string") &&
		typeof candidate.nativeParent === "string" &&
		Array.isArray(candidate.blockers) &&
		candidate.blockers.every((entry) => typeof entry === "string")
	);
}

export function parseSnapshot(snapshotText: string): { snapshot: Snapshot | null; problems: string[] } {
	const problems: string[] = [];
	let parsed: unknown;
	try {
		parsed = JSON.parse(snapshotText);
	} catch (error) {
		return { snapshot: null, problems: [`${SNAPSHOT_PATH} is not valid JSON: ${String(error)}`] };
	}
	if (typeof parsed !== "object" || parsed === null) {
		return { snapshot: null, problems: [`${SNAPSHOT_PATH} is not a JSON object`] };
	}
	const candidate = parsed as Partial<Snapshot>;
	if (!Array.isArray(candidate.records)) {
		return { snapshot: null, problems: [`${SNAPSHOT_PATH} has no records array`] };
	}
	const snapshot: Snapshot = {
		version: typeof candidate.version === "number" ? candidate.version : -1,
		repository: typeof candidate.repository === "string" ? candidate.repository : "",
		canonicalIssue: typeof candidate.canonicalIssue === "number" ? candidate.canonicalIssue : -1,
		sourceHash: typeof candidate.sourceHash === "string" ? candidate.sourceHash : "",
		sourceRecordCount: typeof candidate.sourceRecordCount === "number" ? candidate.sourceRecordCount : -1,
		taskCount: typeof candidate.taskCount === "number" ? candidate.taskCount : -1,
		externalCount: typeof candidate.externalCount === "number" ? candidate.externalCount : -1,
		records: candidate.records.filter(isSnapshotRecord),
	};
	const skipped = candidate.records.length - snapshot.records.length;
	if (skipped > 0) problems.push(`${skipped} snapshot record(s) failed structural validation`);
	return { snapshot, problems };
}

/**
 * Re-derive the expected registry from the published records: every
 * execution record, the prerequisite externals cited (transitively) by
 * execution records, and the synthetic MAP-ROOT anchor for the canonical
 * issue. No shadow graph: rows carry the records verbatim.
 */
export function deriveExpectedRegistry(snapshot: Snapshot): DerivedRegistry {
	const problems: string[] = [];
	const byId = new Map<string, SnapshotRecord>();
	for (const record of snapshot.records) {
		if (byId.has(record.stableId)) problems.push(`duplicate snapshot stable ID ${record.stableId}`);
		byId.set(record.stableId, record);
	}

	const execution = snapshot.records.filter((record) => record.kind === "execution");
	const externals = new Map<string, SnapshotRecord>(
		snapshot.records.filter((record) => record.kind === "external").map((record) => [record.stableId, record]),
	);

	// Prerequisite externals: those cited by execution records, closed under
	// externals' own external blockers. Externals never cite execution nodes.
	const cited = new Set<string>();
	const queue: string[] = [];
	for (const record of execution) {
		for (const blocker of record.blockers) {
			if (externals.has(blocker) && !cited.has(blocker)) {
				cited.add(blocker);
				queue.push(blocker);
			}
		}
	}
	while (queue.length > 0) {
		const current = queue.pop();
		if (current === undefined) break;
		const record = externals.get(current);
		if (record === undefined) continue;
		for (const blocker of record.blockers) {
			if (externals.has(blocker) && !cited.has(blocker)) {
				cited.add(blocker);
				queue.push(blocker);
			}
		}
	}

	const rows: ExpectedRow[] = [];
	for (const record of execution) rows.push(toExpectedRow(record));
	for (const id of [...cited].sort()) {
		const record = externals.get(id);
		if (record !== undefined) rows.push(toExpectedRow(record));
	}
	rows.push({
		stableId: MAP_ROOT_ID,
		recordKind: "map-root",
		issue: snapshot.canonicalIssue,
		title: MAP_ROOT_TITLE,
		blockedBy: [],
		record: null,
	});

	const expectedIds = new Set(rows.map((row) => row.stableId));
	for (const row of rows) {
		for (const blocker of row.blockedBy) {
			if (!expectedIds.has(blocker)) {
				problems.push(`snapshot record ${row.stableId} cites ${blocker}, which is not part of the derived registry`);
			}
		}
	}
	const issues = new Map<number, string>();
	for (const row of rows) {
		const existing = issues.get(row.issue);
		if (existing !== undefined) {
			problems.push(`snapshot issues collide: ${existing} and ${row.stableId} both claim issue #${row.issue}`);
		} else {
			issues.set(row.issue, row.stableId);
		}
	}
	return { rows, problems };
}

function toExpectedRow(record: SnapshotRecord): ExpectedRow {
	return {
		stableId: record.stableId,
		recordKind: record.kind,
		issue: record.issue,
		title: record.title,
		blockedBy: [...record.blockers],
		record,
	};
}

/**
 * Structural sha256 over the mapped snapshot records (stableId, kind, issue,
 * title, blockers in record order), prefixed by the snapshot identity
 * header. Status fields are deliberately excluded so status flips keep the
 * graph check green while any structural hand edit breaks the pin.
 */
export function computeSnapshotStructuralHash(snapshot: Snapshot): string {
	const lines: string[] = [
		"map-ledger-structural-v1",
		`version=${snapshot.version} repository=${snapshot.repository} canonicalIssue=${snapshot.canonicalIssue}`,
	];
	const mapped = deriveExpectedRegistry(snapshot).rows.filter((row) => row.record !== null);
	mapped.sort((left, right) => (left.stableId < right.stableId ? -1 : left.stableId > right.stableId ? 1 : 0));
	for (const row of mapped) {
		lines.push(
			`${row.stableId}\t${row.recordKind}\t#${row.issue}\t${row.title}\t${row.blockedBy.join(",") || "-"}`,
		);
	}
	return createHash("sha256").update(lines.join("\n"), "utf8").digest("hex");
}

/**
 * Canonical JSON mirroring the publisher's `.outline/sdd/workflowz.py`
 * semantics: UTF-8, sorted object keys, compact separators, record and
 * list order preserved.
 */
function canonicalJson(value: unknown): string {
	if (value === null) return "null";
	if (typeof value === "string") return JSON.stringify(value);
	if (typeof value === "boolean") return value ? "true" : "false";
	if (typeof value === "number") {
		if (!Number.isInteger(value)) throw new Error(`cannot canonicalize non-integer number ${value}`);
		return value.toString();
	}
	if (Array.isArray(value)) return `[${value.map((entry) => canonicalJson(entry)).join(",")}]`;
	if (typeof value === "object") {
		const entries = Object.entries(value as Record<string, unknown>).sort(([left], [right]) =>
			left < right ? -1 : left > right ? 1 : 0,
		);
		return `{${entries.map(([key, entry]) => `${JSON.stringify(key)}:${canonicalJson(entry)}`).join(",")}}`;
	}
	throw new Error(`cannot canonicalize value of type ${typeof value}`);
}

/**
 * Publisher-canonical source-record hash over the full structural ticket
 * records (all retained fields, in record order). Mutable issue status is
 * excluded by construction — the tracked witness carries structural fields
 * only — so issue closure never perturbs this provenance hash. Must equal
 * the fixture's declared `sourceHash` and the pinned `SNAPSHOT_SOURCE_HASH`.
 */
export function computeSnapshotSourceHash(records: readonly SnapshotRecord[]): string {
	return `sha256:${createHash("sha256").update(canonicalJson(records), "utf8").digest("hex")}`;
}

// ============================================================================
// Map document and ledger parsing
// ============================================================================

export interface MapRow {
	readonly stableId: string;
	readonly modality: string;
	readonly issue: number;
	readonly title: string;
	readonly blockedBy: readonly string[];
}

export interface ParsedMapDocument {
	readonly rows: readonly MapRow[];
	readonly telemetrySites: readonly string[];
	readonly headerHash: string | null;
	readonly problems: readonly string[];
}

const REGISTRY_ROW_PATTERN = /^\| ([A-Z][A-Z0-9-]*) \| ([a-z]+) \| #(\d+) \| (.*) \| (.*) \|$/;
const TELEMETRY_SITE_PATTERN = /^- (crates\/[^:\s]+):(\d+)-(\d+)$/;
const HEADER_HASH_PATTERN = /Snapshot structural sha256: `?([0-9a-f]{64})`?/;

export function parseExecutionMap(mapText: string): ParsedMapDocument {
	const problems: string[] = [];
	const rows: MapRow[] = [];
	const telemetrySites: string[] = [];
	let headerHash: string | null = null;

	const headerMatch = mapText.match(HEADER_HASH_PATTERN);
	if (headerMatch !== null) headerHash = headerMatch[1] ?? null;

	let section = "preamble";
	for (const rawLine of mapText.split("\n")) {
		const line = rawLine.replace(/\r$/, "");
		if (line.startsWith("## ")) {
			section = line.slice(3).trim();
			continue;
		}
		if (section === "Registry") {
			const match = line.match(REGISTRY_ROW_PATTERN);
			if (match === null) continue;
			const [, stableId, modality, issueText, title, blockedByText] = match;
			if (stableId === undefined || modality === undefined || issueText === undefined) continue;
			const issue = Number.parseInt(issueText, 10);
			if (!Number.isInteger(issue)) continue;
			const blockedBy = (blockedByText ?? "").trim() === "—" ? [] : (blockedByText ?? "").split(", ");
			rows.push({ stableId, modality, issue, title: title ?? "", blockedBy });
		} else if (section === "Pinned telemetry migration surface") {
			const match = line.match(TELEMETRY_SITE_PATTERN);
			if (match === null) continue;
			const [, path, startText, endText] = match;
			if (path === undefined || startText === undefined || endText === undefined) continue;
			telemetrySites.push(`${path}:${startText}-${endText}`);
		}
	}
	if (rows.length === 0) problems.push(`${MAP_DOC_PATH} contains no registry rows`);
	return { rows, telemetrySites, headerHash, problems };
}

export interface LedgerKinds {
	readonly settledKinds: readonly string[];
	readonly parKinds: ReadonlyMap<string, string>;
	readonly problems: readonly string[];
}

const LEDGER_DAG_ROW_PATTERN = /^\| ([A-Z][A-Z0-9-]+) \| ([a-z]+) \| (.*) \|$/;

/** The settled kinds and PAR-track kinds of PARITY_LEDGER's graduated DAG. */
export function parseLedgerSettledKinds(ledgerText: string): LedgerKinds {
	const problems: string[] = [];
	const kinds = new Set<string>();
	const parKinds = new Map<string, string>();
	const section = ledgerText.split("## Graduated parity-ticket DAG")[1] ?? "";
	if (section === "") problems.push(`${PARITY_LEDGER_PATH} has no graduated parity-ticket DAG section`);
	for (const line of section.split("\n")) {
		const match = line.match(LEDGER_DAG_ROW_PATTERN);
		if (match === null) continue;
		const [, stableId, kind] = match;
		if (stableId === undefined || kind === undefined) continue;
		kinds.add(kind);
		if (stableId.startsWith("PAR-")) parKinds.set(stableId, kind);
	}
	if (kinds.size === 0) problems.push(`${PARITY_LEDGER_PATH} graduated DAG produced no settled kinds`);
	return { settledKinds: [...kinds].sort(), parKinds, problems };
}

// ============================================================================
// Graph helpers
// ============================================================================

/** Kahn topological sort over row -> blocker edges; leftover rows are cyclic. */
export function kahnTopologicalOrder(rows: readonly MapRow[]): { order: readonly string[]; cyclic: readonly string[] } {
	const ids = new Set(rows.map((row) => row.stableId));
	const dependents = new Map<string, string[]>();
	const inDegree = new Map<string, number>();
	for (const row of rows) {
		if (!inDegree.has(row.stableId)) inDegree.set(row.stableId, 0);
		for (const blocker of row.blockedBy) {
			if (!ids.has(blocker)) continue;
			inDegree.set(row.stableId, (inDegree.get(row.stableId) ?? 0) + 1);
			const list = dependents.get(blocker);
			if (list === undefined) dependents.set(blocker, [row.stableId]);
			else list.push(row.stableId);
		}
	}
	const queue: string[] = [...inDegree.entries()].filter(([, degree]) => degree === 0).map(([id]) => id);
	const order: string[] = [];
	while (queue.length > 0) {
		const current = queue.shift();
		if (current === undefined) break;
		order.push(current);
		for (const dependent of dependents.get(current) ?? []) {
			const degree = (inDegree.get(dependent) ?? 0) - 1;
			inDegree.set(dependent, degree);
			if (degree === 0) queue.push(dependent);
		}
	}
	const cyclic = [...inDegree.keys()].filter((id) => (inDegree.get(id) ?? 0) > 0);
	return { order, cyclic };
}

/**
 * All stable IDs transitively blocked by `start` (following dependent
 * edges), skipping the IDs in `removed` — the delete-node simulation.
 */
export function dependentClosure(
	rows: readonly MapRow[],
	start: string,
	removed: ReadonlySet<string> = new Set(),
): Set<string> {
	const dependents = new Map<string, string[]>();
	for (const row of rows) {
		for (const blocker of row.blockedBy) {
			const list = dependents.get(blocker);
			if (list === undefined) dependents.set(blocker, [row.stableId]);
			else list.push(row.stableId);
		}
	}
	const seen = new Set<string>();
	const stack: string[] = [start];
	while (stack.length > 0) {
		const current = stack.pop();
		if (current === undefined || seen.has(current) || removed.has(current)) continue;
		seen.add(current);
		for (const dependent of dependents.get(current) ?? []) stack.push(dependent);
	}
	return seen;
}

/**
 * All stable IDs with a dependent path into `start` (following prerequisite
 * edges backward from `start`, which is included) — the terminal-side
 * companion of `dependentClosure`. One traversal per graph, not per row.
 */
export function prerequisiteClosure(rows: readonly MapRow[], start: string): Set<string> {
	const prerequisites = new Map<string, string[]>();
	for (const row of rows) {
		for (const blocker of row.blockedBy) {
			const list = prerequisites.get(row.stableId);
			if (list === undefined) prerequisites.set(row.stableId, [blocker]);
			else list.push(blocker);
		}
	}
	const seen = new Set<string>();
	const stack: string[] = [start];
	while (stack.length > 0) {
		const current = stack.pop();
		if (current === undefined || seen.has(current)) continue;
		seen.add(current);
		for (const prerequisite of prerequisites.get(current) ?? []) stack.push(prerequisite);
	}
	return seen;
}

// ============================================================================
// Individual checks
// ============================================================================

function checkRowSet(docRows: readonly MapRow[], expected: readonly ExpectedRow[]): string[] {
	const violations: string[] = [];
	const docIds = new Set(docRows.map((row) => row.stableId));
	const expectedIds = new Set(expected.map((row) => row.stableId));
	for (const id of [...expectedIds].sort()) {
		if (!docIds.has(id)) violations.push(`registry row ${id} is missing from ${MAP_DOC_PATH}`);
	}
	for (const id of [...docIds].sort()) {
		if (!expectedIds.has(id)) violations.push(`${MAP_DOC_PATH} row ${id} matches no snapshot record`);
	}
	if (expected.length !== EXPECTED_ROW_COUNT) {
		violations.push(
			`derived registry has ${expected.length} rows, expected ${EXPECTED_ROW_COUNT} (109 sibling graduate tickets + 6 map tickets + 7 prerequisite externals + ${MAP_ROOT_ID})`,
		);
	}
	if (docRows.length !== EXPECTED_ROW_COUNT) {
		violations.push(`${MAP_DOC_PATH} has ${docRows.length} rows, expected ${EXPECTED_ROW_COUNT}`);
	}
	return violations;
}

function checkDuplicates(docRows: readonly MapRow[]): string[] {
	const violations: string[] = [];
	const counts = new Map<string, number>();
	for (const row of docRows) counts.set(row.stableId, (counts.get(row.stableId) ?? 0) + 1);
	for (const [id, count] of counts) {
		if (count > 1) violations.push(`stable ID ${id} appears in ${count} rows; each row ID must be unique`);
	}
	return violations;
}

function checkAliases(docRows: readonly MapRow[], expected: readonly ExpectedRow[]): string[] {
	const violations: string[] = [];

	const byIssue = new Map<number, string[]>();
	for (const row of docRows) {
		const list = byIssue.get(row.issue);
		if (list === undefined) byIssue.set(row.issue, [row.stableId]);
		else list.push(row.stableId);
	}
	for (const [issue, ids] of byIssue) {
		if (ids.length > 1) {
			violations.push(`two rows share issue #${issue} (${ids.join(", ")}): one issue, one stable ID`);
		}
	}
	const docRelDocsRows = docRows.filter((row) => row.stableId === REL_DOCS_ID);
	if (docRelDocsRows.length !== 1) {
		violations.push(`${REL_DOCS_ID} must be registered in exactly one row's ID field, found ${docRelDocsRows.length}`);
	}

	const fields: { where: string; text: string }[] = [];
	for (const row of expected) {
		const record = row.record;
		if (record === null) continue;
		// MAP-1's own record enumerates the retired spellings, so it is exempt.
		if (record.stableId === "MAP-1") continue;
		fields.push({ where: `registry ${record.stableId}`, text: record.stableId });
		fields.push({ where: `registry ${record.stableId}`, text: record.title });
		fields.push({ where: `registry ${record.stableId}`, text: record.question ?? "" });
		fields.push({ where: `registry ${record.stableId}`, text: record.acceptance ?? "" });
	}
	for (const row of docRows) {
		fields.push({ where: `${MAP_DOC_PATH} ${row.stableId}`, text: row.stableId });
		fields.push({ where: `${MAP_DOC_PATH} ${row.stableId}`, text: row.title });
		fields.push({ where: `${MAP_DOC_PATH} ${row.stableId}`, text: row.blockedBy.join(", ") });
	}
	for (const field of fields) {
		for (const token of BANNED_ALIAS_TOKENS) {
			if (field.text.includes(token)) {
				violations.push(`banned retired spelling '${token}' appears in ${field.where}`);
			}
		}
		for (const match of field.text.matchAll(BANNED_ALIAS_PATTERN)) {
			violations.push(`banned retired spelling '${match[0]}' appears in ${field.where}`);
		}
		if (field.text.includes(BANNED_PHRASE)) {
			violations.push(`banned phrase '${BANNED_PHRASE}' appears in ${field.where}`);
		}
	}
	return violations;
}

function checkRowFields(docRows: readonly MapRow[], expected: readonly ExpectedRow[], canonicalIssue: number): string[] {
	const violations: string[] = [];
	const expectedById = new Map(expected.map((row) => [row.stableId, row]));
	const seen = new Set<string>();
	for (const row of docRows) {
		if (seen.has(row.stableId)) continue;
		seen.add(row.stableId);
		const wanted = expectedById.get(row.stableId);
		if (wanted === undefined) continue;
		if (row.issue !== wanted.issue) {
			violations.push(`${row.stableId} lists issue #${row.issue}, snapshot says #${wanted.issue}`);
		}
		if (row.title !== wanted.title) {
			violations.push(`${row.stableId} title differs from the snapshot record`);
		}
		const docBlockers = row.blockedBy.join(", ");
		const wantedBlockers = wanted.blockedBy.join(", ");
		if (docBlockers !== wantedBlockers) {
			violations.push(
				`${row.stableId} blocked_by '${docBlockers || "—"}' differs from snapshot '${wantedBlockers || "—"}'`,
			);
		}
	}
	const root = expectedById.get(MAP_ROOT_ID);
	if (root !== undefined && root.issue !== canonicalIssue) {
		violations.push(`${MAP_ROOT_ID} must carry the canonical issue #${canonicalIssue}`);
	}
	return violations;
}

function checkResolution(docRows: readonly MapRow[]): string[] {
	const violations: string[] = [];
	const ids = new Set(docRows.map((row) => row.stableId));
	for (const row of docRows) {
		for (const blocker of row.blockedBy) {
			if (!ids.has(blocker)) {
				violations.push(`${row.stableId} blocked_by references missing node ${blocker}`);
			}
		}
	}
	return violations;
}

/**
 * Anchoring over a verification-only graph. The canonical frontier is
 * derived from the fixture's expected rows (zero blockers, MAP-ROOT
 * excluded) — never from the mutable document — and each frontier row in
 * the document additionally becomes blocked by MAP-ROOT, materializing
 * synthetic `MAP-ROOT -> F` edges without touching published `blocked_by`
 * cells, the fixture, the expected blocker arrays, or either hash. Every
 * published row must then be reachable from MAP-ROOT alone (one dependent
 * closure) and lie on a path into terminal MAP-6 (one prerequisite
 * closure); a document-only zero-blocker row gets no synthetic edge and
 * cannot promote itself into the root set.
 */
function checkAnchoring(docRows: readonly MapRow[], expected: readonly ExpectedRow[]): string[] {
	const violations: string[] = [];
	const ids = new Set(docRows.map((row) => row.stableId));
	if (!ids.has(MAP_ROOT_ID)) {
		violations.push(`${MAP_ROOT_ID} row is missing; it anchors the zero-prerequisite frontier`);
		return violations;
	}
	if (!ids.has(TERMINAL_NODE_ID)) {
		violations.push(`${TERMINAL_NODE_ID} (the terminal closure node) is missing; every row must reach it`);
		return violations;
	}
	const frontier = new Set(
		expected
			.filter((row) => row.stableId !== MAP_ROOT_ID && row.blockedBy.length === 0)
			.map((row) => row.stableId),
	);
	const verificationRows: MapRow[] = docRows.map((row) =>
		frontier.has(row.stableId) ? { ...row, blockedBy: [MAP_ROOT_ID, ...row.blockedBy] } : row,
	);
	const reachable = dependentClosure(verificationRows, MAP_ROOT_ID);
	const reachesTerminal = prerequisiteClosure(verificationRows, TERMINAL_NODE_ID);
	for (const row of docRows) {
		const reasons: string[] = [];
		if (!reachable.has(row.stableId)) reasons.push(`not reachable from ${MAP_ROOT_ID}`);
		if (!reachesTerminal.has(row.stableId)) reasons.push(`no path into terminal ${TERMINAL_NODE_ID}`);
		if (reasons.length > 0) violations.push(`orphan row ${row.stableId}: ${reasons.join(" and ")}`);
	}
	return violations;
}

function checkModalities(
	docRows: readonly MapRow[],
	ledger: LedgerKinds,
	expected: readonly ExpectedRow[],
): string[] {
	const violations: string[] = [...ledger.problems];
	const settled = new Set(ledger.settledKinds);
	for (const row of docRows) {
		if (!settled.has(row.modality)) {
			violations.push(`${row.stableId} modality '${row.modality}' is not a PARITY_LEDGER settled kind`);
		}
	}
	for (const [stableId, kind] of ledger.parKinds) {
		const row = docRows.find((entry) => entry.stableId === stableId);
		if (row === undefined) continue;
		if (row.modality !== kind) {
			violations.push(`${stableId} modality '${row.modality}' contradicts PARITY_LEDGER kind '${kind}'`);
		}
	}
	for (const row of docRows) {
		if (row.stableId.startsWith("EXT-") && row.modality !== "external") {
			violations.push(`${row.stableId} is an external node and must use modality 'external'`);
		}
	}
	const rootRow = docRows.find((entry) => entry.stableId === MAP_ROOT_ID);
	if (rootRow !== undefined && rootRow.modality !== "task") {
		violations.push(`${MAP_ROOT_ID} must use modality 'task'`);
	}
	const externalKind = "external";
	for (const kind of settled) {
		if (kind === externalKind) continue;
		if (!docRows.some((row) => row.modality === kind)) {
			violations.push(`graduation modality '${kind}' has no rows; all four must stay populated`);
		}
	}
	if (!expected.some((row) => row.recordKind === "external")) {
		violations.push("derived registry contains no external rows");
	}
	return violations;
}

function checkClosureComposition(docRows: readonly MapRow[]): string[] {
	const violations: string[] = [];
	const byId = new Map(docRows.map((row) => [row.stableId, row]));

	const relClose = byId.get(REL_CLOSE_ID);
	if (relClose !== undefined && !relClose.blockedBy.includes(REL_DOCS_ID)) {
		violations.push(`required closure edge ${REL_CLOSE_ID} <- ${REL_DOCS_ID} is missing`);
	}
	const docF = byId.get(DOC_F_ID);
	if (docF !== undefined) {
		for (const prerequisite of [...SEVEN_CLOSERS.filter((closer) => closer !== DOC_F_ID), REL_DOCS_ID]) {
			if (!docF.blockedBy.includes(prerequisite)) {
				violations.push(`required prerequisite ${prerequisite} of ${DOC_F_ID} is missing`);
			}
		}
	}
	const map5 = byId.get(CLOSURE_GATE_NODE_ID);
	if (map5 !== undefined) {
		for (const closer of ["MAP-4", ...SEVEN_CLOSERS]) {
			if (!map5.blockedBy.includes(closer)) {
				violations.push(`${CLOSURE_GATE_NODE_ID} closure composition is missing ${closer}`);
			}
		}
	}
	const map6 = byId.get(TERMINAL_NODE_ID);
	if (map6 !== undefined) {
		const blockers = map6.blockedBy.join(", ");
		if (blockers !== CLOSURE_GATE_NODE_ID) {
			violations.push(
				`${TERMINAL_NODE_ID} must be blocked by exactly ${CLOSURE_GATE_NODE_ID}, found '${blockers || "—"}'`,
			);
		}
	}
	return violations;
}

/**
 * REL-DOCS dominance: no REL-* node may reach DOC-F around the
 * REL-DOCS/REL-CLOSE gate. The check is a delete-node simulation (remove the
 * gate, assert disconnect) plus non-vacuity: some non-gate REL-* node must
 * reach DOC-F in the intact graph, and each single-gate deletion must leave
 * a path, so only the joint gate is load-bearing.
 */
export function checkRelDocsDominance(docRows: readonly MapRow[]): string[] {
	const violations: string[] = [];
	const byId = new Map(docRows.map((row) => [row.stableId, row]));
	const docF = byId.get(DOC_F_ID);
	if (docF === undefined) return violations;
	const gate = new Set<string>([REL_DOCS_ID, REL_CLOSE_ID]);
	const releaseNodes = docRows.filter((row) => /^REL-/.test(row.stableId)).map((row) => row.stableId);

	for (const id of releaseNodes) {
		if (gate.has(id)) continue;
		if (dependentClosure(docRows, id, gate).has(DOC_F_ID)) {
			violations.push(
				`release node ${id} reaches ${DOC_F_ID} around the ${REL_DOCS_ID}/${REL_CLOSE_ID} gate (delete-node simulation)`,
			);
		}
	}

	const nonGateReachesDocF = (removed: ReadonlySet<string>): boolean =>
		releaseNodes.some((id) => !gate.has(id) && !removed.has(id) && dependentClosure(docRows, id, removed).has(DOC_F_ID));

	if (!nonGateReachesDocF(new Set())) {
		violations.push(
			`dominance check is vacuous: no non-gate REL-* node reaches ${DOC_F_ID} in the intact graph`,
		);
	}
	if (!nonGateReachesDocF(new Set([REL_DOCS_ID]))) {
		violations.push(`deleting only ${REL_DOCS_ID} already disconnects ${DOC_F_ID}; the gate is not two-member`);
	}
	if (!nonGateReachesDocF(new Set([REL_CLOSE_ID]))) {
		violations.push(`deleting only ${REL_CLOSE_ID} already disconnects ${DOC_F_ID}; the gate is not two-member`);
	}
	return violations;
}

function checkTelemetryPin(docRows: readonly MapRow[], telemetrySites: readonly string[]): string[] {
	const violations: string[] = [];
	const documented = new Set(telemetrySites);
	if (documented.size !== telemetrySites.length) {
		violations.push("documented telemetry sites contain duplicates");
	}
	for (const site of telemetrySites) {
		if (!PINNED_AGENT_LOOP_CONFIG_SITES.some((pin) => `${pin.path}:${pin.start}-${pin.end}` === site)) {
			violations.push(`documented site ${site} matches no pinned AgentLoopConfig site`);
		}
	}
	for (const pin of PINNED_AGENT_LOOP_CONFIG_SITES) {
		const site = `${pin.path}:${pin.start}-${pin.end}`;
		if (!documented.has(site)) {
			violations.push(`pinned AgentLoopConfig site ${site} is missing from ${MAP_DOC_PATH}`);
		}
	}
	if (telemetrySites.length !== PINNED_AGENT_LOOP_CONFIG_SITES.length) {
		violations.push(
			`expected exactly ${PINNED_AGENT_LOOP_CONFIG_SITES.length} documented telemetry sites, found ${telemetrySites.length}`,
		);
	}
	if (docRows.length > 0 && telemetrySites.length === 0) {
		violations.push(`${MAP_DOC_PATH} has no pinned telemetry migration surface`);
	}
	return violations;
}

// ============================================================================
// Orchestration
// ============================================================================

export interface MapLedgerInputs {
	readonly snapshotText: string;
	readonly ledgerText: string;
	readonly mapText: string;
}

/** Run every map-ledger assertion; an empty list means green. */
export function runMapLedgerChecks(inputs: MapLedgerInputs): string[] {
	const violations: string[] = [];
	const add = (witness: string, results: readonly string[]): void => {
		for (const result of results) violations.push(`[${witness}] ${result}`);
	};

	const parsed = parseSnapshot(inputs.snapshotText);
	add("snapshot", parsed.problems);
	const snapshot = parsed.snapshot;
	if (snapshot === null) return violations;

	if (snapshot.version !== 2) add("snapshot", [`${SNAPSHOT_PATH} version is ${snapshot.version}, expected 2`]);
	if (snapshot.repository !== "metaphorics/pi-oxidized") {
		add("snapshot", [`${SNAPSHOT_PATH} repository is '${snapshot.repository}', expected 'metaphorics/pi-oxidized'`]);
	}
	if (snapshot.canonicalIssue !== 12) {
		add("snapshot", [`${SNAPSHOT_PATH} canonicalIssue is ${snapshot.canonicalIssue}, expected 12`]);
	}
	if (snapshot.sourceHash !== SNAPSHOT_SOURCE_HASH) {
		add("source-hash", [
			`${SNAPSHOT_PATH} sourceHash '${snapshot.sourceHash}' does not match the pinned ${SNAPSHOT_SOURCE_HASH}; the witness was not published by the live authority`,
		]);
	}
	try {
		const recomputedSourceHash = computeSnapshotSourceHash(snapshot.records);
		if (recomputedSourceHash !== snapshot.sourceHash) {
			add("source-hash", [
				`recomputed source-record hash ${recomputedSourceHash} does not match the declared ${snapshot.sourceHash}; structural ticket records were edited after publication`,
			]);
		}
	} catch (error) {
		add("source-hash", [`cannot canonicalize structural ticket records: ${String(error)}`]);
	}
	const executionCount = snapshot.records.filter((record) => record.kind === "execution").length;
	const externalCount = snapshot.records.filter((record) => record.kind === "external").length;
	if (snapshot.sourceRecordCount !== snapshot.records.length) {
		add("source-hash", [
			`${SNAPSHOT_PATH} sourceRecordCount is ${snapshot.sourceRecordCount} but ${snapshot.records.length} record(s) parsed`,
		]);
	}
	if (snapshot.sourceRecordCount !== EXPECTED_SOURCE_RECORD_COUNT) {
		add("source-hash", [
			`${SNAPSHOT_PATH} sourceRecordCount is ${snapshot.sourceRecordCount}, expected ${EXPECTED_SOURCE_RECORD_COUNT}`,
		]);
	}
	if (snapshot.taskCount !== executionCount) {
		add("source-hash", [
			`${SNAPSHOT_PATH} taskCount is ${snapshot.taskCount} but ${executionCount} execution record(s) parsed`,
		]);
	}
	if (snapshot.taskCount !== EXPECTED_TASK_COUNT) {
		add("source-hash", [`${SNAPSHOT_PATH} taskCount is ${snapshot.taskCount}, expected ${EXPECTED_TASK_COUNT}`]);
	}
	if (snapshot.externalCount !== externalCount) {
		add("source-hash", [
			`${SNAPSHOT_PATH} externalCount is ${snapshot.externalCount} but ${externalCount} external record(s) parsed`,
		]);
	}
	if (snapshot.externalCount !== EXPECTED_EXTERNAL_COUNT) {
		add("source-hash", [
			`${SNAPSHOT_PATH} externalCount is ${snapshot.externalCount}, expected ${EXPECTED_EXTERNAL_COUNT}`,
		]);
	}

	const derived = deriveExpectedRegistry(snapshot);
	add("expected-registry", derived.problems);

	const structuralHash = computeSnapshotStructuralHash(snapshot);
	if (structuralHash !== SNAPSHOT_STRUCTURAL_SHA256) {
		add("structural-hash", [
			`recomputed snapshot structural sha256 ${structuralHash} does not match the pinned ${SNAPSHOT_STRUCTURAL_SHA256}; the snapshot was edited outside the publishing flow`,
		]);
	}

	const doc = parseExecutionMap(inputs.mapText);
	add("map-document", doc.problems);
	if (doc.headerHash !== null && doc.headerHash !== SNAPSHOT_STRUCTURAL_SHA256) {
		add("structural-hash", [
			`${MAP_DOC_PATH} header hash ${doc.headerHash} does not match the pinned ${SNAPSHOT_STRUCTURAL_SHA256}`,
		]);
	}

	const ledger = parseLedgerSettledKinds(inputs.ledgerText);

	add("row-set", checkRowSet(doc.rows, derived.rows));
	add("duplicate", checkDuplicates(doc.rows));
	add("alias", checkAliases(doc.rows, derived.rows));
	add("row-match", checkRowFields(doc.rows, derived.rows, snapshot.canonicalIssue));
	add("resolution", checkResolution(doc.rows));
	const kahn = kahnTopologicalOrder(doc.rows);
	if (kahn.cyclic.length > 0) {
		add("acyclicity", [`blocked_by graph contains a cycle involving: ${kahn.cyclic.join(", ")}`]);
	}
	add("anchoring", checkAnchoring(doc.rows, derived.rows));
	add("modality", checkModalities(doc.rows, ledger, derived.rows));
	add("closure", checkClosureComposition(doc.rows));
	add("rel-docs-dominance", checkRelDocsDominance(doc.rows));
	add("telemetry", checkTelemetryPin(doc.rows, doc.telemetrySites));

	return violations;
}

/**
 * Read the three required inputs; the tracked witness is read first so a
 * fresh checkout without ignored workflow state reports the exact missing
 * fixture. Any read failure throws one stable diagnostic naming the
 * repo-relative path, keeping `runMapLedgerChecks` pure over text.
 */
export function loadMapLedgerInputs(repoRoot: string): MapLedgerInputs {
	const readRequired = (relativePath: string): string => {
		try {
			return readFileSync(join(repoRoot, relativePath), "utf8");
		} catch (error) {
			throw new Error(`cannot read required ${relativePath}: ${String(error)}`);
		}
	};
	return {
		snapshotText: readRequired(SNAPSHOT_PATH),
		ledgerText: readRequired(PARITY_LEDGER_PATH),
		mapText: readRequired(MAP_DOC_PATH),
	};
}

function main(): void {
	let inputs: MapLedgerInputs;
	try {
		inputs = loadMapLedgerInputs(REPO_ROOT);
	} catch (error) {
		console.error(`map ledger input failed: ${error instanceof Error ? error.message : String(error)}`);
		process.exit(1);
	}
	const violations = runMapLedgerChecks(inputs);
	if (violations.length > 0) {
		console.error(`map ledger failed with ${violations.length} violation(s):`);
		for (const violation of violations) console.error(`  - ${violation}`);
		process.exit(1);
	}
	process.stdout.write("MAP_LEDGER_OK\n");
}

if (import.meta.main) main();
