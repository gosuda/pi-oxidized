#!/usr/bin/env bun
/**
 * Fog-graduation and decision-index lint (MAP-3, issue #141).
 *
 * The execution map stays a single graph authority: this tool imports the
 * MAP-1 ledger (`map.ts`) and consumes its published artifacts — the
 * structural ticket-record witness, docs/EXECUTION_MAP.md, and
 * docs/PARITY_LEDGER.md — through `loadMapLedgerInputs`. It never re-parses
 * the registry or re-derives edges; every graph assertion here is a re-run
 * of `runMapLedgerChecks` over the unchanged published texts.
 *
 * The lint owns three contracts:
 *
 * 1. fog graduation   - every shipped-surface node (an `execution` record)
 *                       is graduated in shape: a published question and an
 *                       acceptance contract. A record without either is
 *                       ungraduated fog, so the check reports zero on the
 *                       final records and fails loudly the moment a stub is
 *                       published. Externals are prerequisite research
 *                       questions and carry `acceptance: null` by design.
 * 2. decision index   - the append-only Decisions-so-far index comment on
 *                       the canonical issue (#12) covers exactly the closed
 *                       records: every closed record is indexed with its
 *                       owning stable ID, no indexed decision is a phantom,
 *                       and no row indexes a still-open ticket.
 * 3. status flips     - live issue state is classification only; it is
 *                       intentionally absent from the structural witness,
 *                       so a status flip never perturbs the published
 *                       records. `--flip <STABLE_ID>` dry-runs one flip and
 *                       re-runs the full graph check against the unchanged
 *                       published records, still reporting zero alias and
 *                       zero REL-DOCS-bypass violations.
 *
 * `bun run verify:fog` prints FOG_LINT_OK when the records are graduated,
 * the graph baseline is green, the live census classifies every record, and
 * the decision index coverage is exact. `--close-gate` additionally demands
 * a fully graduated shipped surface (zero open `execution` nodes) — it stays
 * red and blocks #12 closure while any shipped-surface fog node remains
 * ungraduated. `--flip <STABLE_ID>` performs the dry-run status flip.
 *
 * Checks are pure over their inputs; only `loadLiveIssueStates` and
 * `loadCanonicalIssueComments` shell out to `gh`.
 */

import {
	SNAPSHOT_STRUCTURAL_SHA256,
	computeSnapshotStructuralHash,
	loadMapLedgerInputs,
	parseSnapshot,
	runMapLedgerChecks,
	type MapLedgerInputs,
	type Snapshot,
} from "./map.ts";
import { REPO_ROOT } from "./parity.ts";

// ============================================================================
// Live issue state
// ============================================================================

export type IssueState = "open" | "closed";
export type IssueStates = ReadonlyMap<number, IssueState>;

/**
 * Fetch the live state of every issue named in `issues` from
 * `repo` via `gh api` (REST, paginated; pull requests skipped). Any
 * transport failure or missing issue throws one stable diagnostic.
 */
export function loadLiveIssueStates(repo: string, issues: readonly number[]): IssueStates {
	const wanted = new Set(issues);
	const states = new Map<number, IssueState>();
	for (let page = 1; page <= 20; page++) {
		const result = Bun.spawnSync(
			["gh", "api", `repos/${repo}/issues?state=all&per_page=100&page=${page}`],
			{ stdout: "pipe", stderr: "pipe" },
		);
		if (result.exitCode !== 0) {
			throw new Error(
				`gh api issues page ${page} failed (${result.exitCode}): ${new TextDecoder().decode(result.stderr).trim()}`,
			);
		}
		const rows = JSON.parse(new TextDecoder().decode(result.stdout)) as unknown;
		if (!Array.isArray(rows)) throw new Error(`gh api issues page ${page} returned a non-array payload`);
		for (const row of rows) {
			if (typeof row !== "object" || row === null) continue;
			const entry = row as Record<string, unknown>;
			if ("pull_request" in entry) continue;
			const number = entry["number"];
			const state = entry["state"];
			if (typeof number !== "number" || (state !== "open" && state !== "closed")) continue;
			if (wanted.has(number)) states.set(number, state);
		}
		if (rows.length < 100) break;
	}
	const missing = issues.filter((issue) => !states.has(issue));
	if (missing.length > 0) throw new Error(`gh api returned no state for issue(s): ${missing.join(", ")}`);
	return states;
}

/**
 * Fetch every comment body on the canonical issue so the decision index
 * can be located offline-testably (callers inject comment texts in tests).
 */
export function loadCanonicalIssueComments(repo: string, canonicalIssue: number): string[] {
	const result = Bun.spawnSync(
		["gh", "issue", "view", String(canonicalIssue), "--repo", repo, "--json", "comments"],
		{ stdout: "pipe", stderr: "pipe" },
	);
	if (result.exitCode !== 0) {
		throw new Error(
			`gh issue view ${canonicalIssue} failed (${result.exitCode}): ${new TextDecoder().decode(result.stderr).trim()}`,
		);
	}
	const payload = JSON.parse(new TextDecoder().decode(result.stdout)) as Record<string, unknown>;
	const comments = payload["comments"];
	if (!Array.isArray(comments)) throw new Error(`gh issue view ${canonicalIssue} returned no comments array`);
	return comments.map((comment) => {
		const body = (comment as Record<string, unknown>)["body"];
		return typeof body === "string" ? body : "";
	});
}

// ============================================================================
// Fog graduation
// ============================================================================

/**
 * Zero ungraduated shipped-surface fog nodes on the published records:
 * every `execution` record carries a question and an acceptance contract
 * (the graduation shape #12 requires — "each resolving ticket must
 * graduate newly sharp work before it closes").
 */
export function checkFogGraduation(snapshot: Snapshot): string[] {
	const violations: string[] = [];
	for (const record of snapshot.records) {
		if (record.kind !== "execution") continue;
		if ((record.question ?? "").trim() === "") {
			violations.push(`${record.stableId}: shipped-surface node publishes no question — ungraduated fog`);
		}
		if ((record.acceptance ?? "").trim() === "") {
			violations.push(`${record.stableId}: shipped-surface node publishes no acceptance contract — ungraduated fog`);
		}
	}
	return violations;
}

export interface GraduationCensus {
	/** Closed `execution` nodes — shipped surface that graduated. */
	readonly graduated: readonly string[];
	/** Open `execution` nodes — ungraduated shipped surface; blocks #12 closure at gate time. */
	readonly open: readonly string[];
	/** Every closed record (`execution` and `external`) — the exact decision-index coverage set. */
	readonly closedDecisions: readonly string[];
	/** Records whose live state is missing, so classification is impossible. */
	readonly unknownState: readonly string[];
}

/** Classify every record against live states; pure over its inputs. */
export function graduationCensus(snapshot: Snapshot, states: IssueStates): GraduationCensus {
	const graduated: string[] = [];
	const open: string[] = [];
	const closedDecisions: string[] = [];
	const unknownState: string[] = [];
	for (const record of snapshot.records) {
		const state = states.get(record.issue);
		if (state === undefined) {
			unknownState.push(record.stableId);
			continue;
		}
		if (state === "closed") {
			closedDecisions.push(record.stableId);
			if (record.kind === "execution") graduated.push(record.stableId);
		} else if (record.kind === "execution") {
			open.push(record.stableId);
		}
	}
	return { graduated, open, closedDecisions, unknownState };
}

/**
 * The #12 closure gate: red while any shipped-surface fog node is still
 * ungraduated (an open `execution` node), naming every blocker.
 */
export function checkCloseGate(census: GraduationCensus): string[] {
	if (census.open.length === 0) return [];
	return [
		`[close-gate] ${census.open.length} shipped-surface fog node(s) remain ungraduated: ${census.open.join(", ")}`,
	];
}

// ============================================================================
// Decision index
// ============================================================================

/** Header line marking the append-only index comment on the canonical issue. */
export const DECISION_INDEX_MARKER = "## Decisions-so-far index (append-only)";

const INDEX_ROW_PATTERN = /^\|\s*([A-Z][A-Z0-9-]+)\s*\|/;

/**
 * The index is one append-only comment: more than one comment carrying the
 * marker fragments the record and fails the lint.
 */
export function checkSingleIndexComment(comments: readonly string[]): string[] {
	const matches = comments.filter((body) => body.includes(DECISION_INDEX_MARKER));
	if (matches.length > 1) {
		return [
			`canonical issue carries ${matches.length} Decisions-so-far index comments; exactly one append-only index comment is allowed`,
		];
	}
	return [];
}

/** Locate the comment carrying the index marker (last one wins); null when absent. */
export function findDecisionIndexComment(comments: readonly string[]): string | null {
	const matches = comments.filter((body) => body.includes(DECISION_INDEX_MARKER));
	if (matches.length === 0) return null;
	return matches[matches.length - 1] ?? null;
}

/**
 * The index covers exactly the closed records: every closed record is
 * indexed with its owning stable ID, no indexed decision is a phantom, no
 * row indexes a still-open ticket, and unclassifiable records are reported.
 */
export function checkDecisionIndex(
	indexText: string | null,
	snapshot: Snapshot,
	states: IssueStates,
): string[] {
	const violations: string[] = [];
	const byStableId = new Map(snapshot.records.map((record) => [record.stableId, record]));
	if (indexText === null) {
		return ["canonical issue carries no Decisions-so-far index comment"];
	}
	const rowCount = new Map<string, number>();
	for (const line of indexText.split("\n")) {
		const match = INDEX_ROW_PATTERN.exec(line);
		if (match === null) continue;
		const stableId = match[1] ?? "";
		rowCount.set(stableId, (rowCount.get(stableId) ?? 0) + 1);
		const record = byStableId.get(stableId);
		if (record === undefined) {
			violations.push(`${stableId}: indexed decision matches no registry record (phantom decision row)`);
			continue;
		}
		const state = states.get(record.issue);
		if (state === undefined) {
			violations.push(`${stableId}: owning ticket #${record.issue} has no live state; cannot classify`);
		} else if (state === "open") {
			violations.push(`${stableId}: index row names still-open ticket #${record.issue}; only closed decisions belong`);
		}
	}
	for (const [stableId, count] of rowCount) {
		if (count > 1) {
			violations.push(`${stableId}: indexed ${count} times; the append-only index carries exactly one row per decision`);
		}
	}
	for (const record of snapshot.records) {
		const state = states.get(record.issue);
		if (state === undefined) {
			violations.push(`${record.stableId}: no live state; the index cannot prove coverage`);
		} else if (state === "closed" && !rowCount.has(record.stableId)) {
			violations.push(`${record.stableId}: closed decision is missing from the index`);
		}
	}
	return violations;
}

// ============================================================================
// Dry-run status flip
// ============================================================================

/** Flip one issue's state in the overlay (open<->closed); throws on unknown issue. */
export function flipStatus(states: IssueStates, issue: number): IssueStates {
	const current = states.get(issue);
	if (current === undefined) throw new Error(`cannot flip unknown issue #${issue}`);
	const flipped = new Map(states);
	flipped.set(issue, current === "open" ? "closed" : "open");
	return flipped;
}

export interface FlipRecheck {
	/** Human-readable flip, e.g. `MAP-3 (#141): open -> closed (dry-run)`. */
	readonly flipped: string;
	/** Full graph check re-run against the unchanged published records. */
	readonly graphViolations: readonly string[];
	readonly aliasViolations: readonly string[];
	readonly relDocsBypassViolations: readonly string[];
	/** Structural hash recomputed from the unchanged records during the re-run. */
	readonly structuralSha256: string;
	/** Census re-classified under the flipped overlay. */
	readonly census: GraduationCensus;
}

/**
 * Dry-run one status flip and re-run the graph check against the unchanged
 * published records: the witness intentionally carries no mutable status,
 * so the ledger (alias and REL-DOCS-bypass assertions included) must stay
 * green with the structural hash still pinned. Pure over its inputs.
 */
export function dryRunStatusFlip(
	inputs: MapLedgerInputs,
	snapshot: Snapshot,
	states: IssueStates,
	stableId: string,
): FlipRecheck {
	const record = snapshot.records.find((candidate) => candidate.stableId === stableId);
	if (record === undefined) throw new Error(`unknown stable ID '${stableId}'`);
	const before = states.get(record.issue);
	if (before === undefined) throw new Error(`${stableId}: no live state for #${record.issue}; cannot flip`);
	const after = before === "open" ? "closed" : "open";
	const graphViolations = runMapLedgerChecks(inputs);
	return {
		flipped: `${stableId} (#${record.issue}): ${before} -> ${after} (dry-run)`,
		graphViolations,
		aliasViolations: graphViolations.filter((violation) => violation.startsWith("[alias]")),
		relDocsBypassViolations: graphViolations.filter((violation) =>
			violation.startsWith("[rel-docs-dominance]"),
		),
		structuralSha256: computeSnapshotStructuralHash(snapshot),
		census: graduationCensus(snapshot, flipStatus(states, record.issue)),
	};
}

// ============================================================================
// Orchestration
// ============================================================================

export function runFogLintChecks(
	inputs: MapLedgerInputs,
	states: IssueStates,
	indexText: string | null,
): { violations: string[]; census: GraduationCensus } {
	const violations: string[] = [];
	const add = (witness: string, results: readonly string[]): void => {
		for (const result of results) violations.push(`[${witness}] ${result}`);
	};

	const baseline = runMapLedgerChecks(inputs);
	add("graph-baseline", baseline);

	const parsed = parseSnapshot(inputs.snapshotText);
	add("snapshot", parsed.problems);
	const snapshot = parsed.snapshot;
	if (snapshot === null) throw new Error(`cannot parse the published ticket-record witness`);

	add("fog-graduation", checkFogGraduation(snapshot));

	const census = graduationCensus(snapshot, states);
	if (census.unknownState.length > 0) {
		add("census", [`no live state for: ${census.unknownState.join(", ")}`]);
	}
	add("decision-index", checkDecisionIndex(indexText, snapshot, states));

	return { violations, census };
}

function main(): void {
	const args = process.argv.slice(2);
	const flipIndex = args.indexOf("--flip");
	const flipNext = flipIndex >= 0 ? args[flipIndex + 1] : undefined;
	const flipId = flipNext !== undefined && !flipNext.startsWith("--") ? flipNext : undefined;
	if (flipIndex >= 0 && flipId === undefined) {
		console.error("usage: bun run verify:fog [--flip <STABLE_ID>] [--close-gate] — --flip needs a stable ID");
		process.exit(2);
	}
	const closeGate = args.includes("--close-gate");

	let inputs: MapLedgerInputs;
	try {
		inputs = loadMapLedgerInputs(REPO_ROOT);
	} catch (error) {
		console.error(`fog lint input failed: ${error instanceof Error ? error.message : String(error)}`);
		process.exit(1);
	}
	const parsed = parseSnapshot(inputs.snapshotText);
	if (parsed.snapshot === null) {
		console.error(`fog lint cannot parse the witness: ${parsed.problems.join("; ")}`);
		process.exit(1);
	}
	const snapshot = parsed.snapshot;

	let states: IssueStates;
	let indexText: string | null;
	let comments: string[];
	try {
		states = loadLiveIssueStates(snapshot.repository, [
			...new Set(snapshot.records.map((record) => record.issue)),
		]);
		comments = loadCanonicalIssueComments(snapshot.repository, snapshot.canonicalIssue);
		indexText = findDecisionIndexComment(comments);
	} catch (error) {
		console.error(`fog lint live-state load failed: ${error instanceof Error ? error.message : String(error)}`);
		process.exit(1);
	}

	const { violations, census } = runFogLintChecks(inputs, states, indexText);
	const failures: string[] = [...violations, ...checkSingleIndexComment(comments)];

	if (flipId !== undefined) {
		let flip: FlipRecheck;
		try {
			flip = dryRunStatusFlip(inputs, snapshot, states, flipId);
		} catch (error) {
			console.error(`dry-run flip failed: ${error instanceof Error ? error.message : String(error)}`);
			process.exit(1);
		}
		process.stdout.write(`FLIP ${flip.flipped}\n`);
		if (flip.structuralSha256 !== SNAPSHOT_STRUCTURAL_SHA256) {
			failures.push(`[flip] structural sha256 changed to ${flip.structuralSha256}; published records were perturbed`);
		}
		if (flip.aliasViolations.length > 0 || flip.relDocsBypassViolations.length > 0) {
			failures.push(
				`[flip] graph re-run after the flip reports ${flip.aliasViolations.length} alias and ${flip.relDocsBypassViolations.length} REL-DOCS-bypass violation(s)`,
			);
		}
		process.stdout.write(
			`GRAPH_RERUN violations=${flip.graphViolations.length} aliases=${flip.aliasViolations.length} rel_docs_bypass=${flip.relDocsBypassViolations.length} structural_sha256=${flip.structuralSha256}\n`,
		);
		process.stdout.write(
			`CENSUS_AFTER_FLIP graduated=${flip.census.graduated.length} open=${flip.census.open.length} closed_decisions=${flip.census.closedDecisions.length}\n`,
		);
	}

	if (closeGate) failures.push(...checkCloseGate(census));

	if (failures.length > 0) {
		console.error(`fog lint failed with ${failures.length} violation(s):`);
		for (const failure of failures) console.error(`  - ${failure}`);
		process.exit(1);
	}

	process.stdout.write(
		`FOG_CENSUS graduated=${census.graduated.length} open=${census.open.length} closed_decisions=${census.closedDecisions.length}\n`,
	);
	if (census.open.length > 0) {
		const preview = census.open.slice(0, 12).join(", ");
		const suffix = census.open.length > 12 ? `, +${census.open.length - 12} more` : "";
		process.stdout.write(`OPEN_FRONTIER ${preview}${suffix} — #12 closure gate stays blocked until zero\n`);
	}
	if (flipId !== undefined) process.stdout.write("FLIP_OK\n");
	if (closeGate) process.stdout.write("CLOSE_GATE_OK\n");
	process.stdout.write("FOG_LINT_OK\n");
}

if (import.meta.main) main();
