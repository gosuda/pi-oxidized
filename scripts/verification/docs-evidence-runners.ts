/**
 * Seven closed evidence-class runners for the doc-evidence checker (DOC-A, issue #129).
 *
 * Each runner accepts a ledger row and the repository root, validates the row's
 * class-specific params, computes a fresh contentHash for the surface, and
 * returns a sidecar binding (contentHash + toolVersion + runId).  A runner
 * never shells out and never reads a command/argv string from the row — the
 * closed class set and param shapes are the only inputs.
 *
 * The seven closed classes:
 *   1. version-pin         — a version constant value pinned at a source path
 *   2. generated-block      — a generated artifact traced to its generator source
 *   3. fenced-compile       — a fenced code block in a docs topic (path-registered)
 *   4. transcript-claim     — a CLI help surface claiming a specific string
 *   5. matrix-count         — a matrix file with an expected row/item count
 *   6. review-only-prose    — a prose surface that sync-docs may not auto-edit
 *   7. changelog-unreleased — a CHANGELOG with a ## [Unreleased] append slot
 */

import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export const SCHEMA = "pi.docs.evidence.v1" as const;
export const TOOL_VERSION = "pi.docs.evidence.v1" as const;

export const EVIDENCE_CLASSES = [
	"version-pin",
	"generated-block",
	"fenced-compile",
	"transcript-claim",
	"matrix-count",
	"review-only-prose",
	"changelog-unreleased",
] as const;

export type EvidenceClass = (typeof EVIDENCE_CLASSES)[number];

/**
 * Row evidence lifecycle: the row's documentation surface is verified today
 * (`present`), not yet ported (`pending-port`), or exists but lacks fresh
 * evidence (`pending-evidence`).
 */
export const EVIDENCE_STATUSES = [
	"present",
	"pending-port",
	"pending-evidence",
] as const;

export type EvidenceStatus = (typeof EVIDENCE_STATUSES)[number];

export function isEvidenceStatus(value: unknown): value is EvidenceStatus {
	return typeof value === "string" && (EVIDENCE_STATUSES as readonly string[]).includes(value);
}

/** Field names that no ledger row may carry (no command/argv strings). */
export const FORBIDDEN_FIELDS = [
	"command",
	"argv",
	"cmd",
	"args",
	"shell",
	"exec",
] as const;

/** Default re-proof interval in milliseconds (7 days). */
export const DEFAULT_REPROOF_INTERVAL_MS = 7 * 24 * 60 * 60 * 1000;

export interface LedgerRow {
	readonly id: string;
	readonly surface: string;
	readonly owner: string;
	readonly status: EvidenceStatus;
	readonly target: string;
	readonly class: EvidenceClass;
	readonly params: Readonly<Record<string, unknown>>;
}

export interface Sidecar {
	readonly rowId: string;
	readonly contentHash: string;
	readonly toolVersion: string;
	readonly runId: string;
}

export interface RunnerResult {
	readonly ok: boolean;
	readonly sidecar: Sidecar;
	readonly problems: readonly string[];
}

// ---------------------------------------------------------------------------
// Run manifest (pi.docs.evidence.run.v1)
// ---------------------------------------------------------------------------

export const RUN_MANIFEST_SCHEMA = "pi.docs.evidence.run.v1" as const;

/** One ledger row's fresh evidence, as recorded in the run manifest. */
export interface RunManifestEntry {
	readonly rowId: string;
	readonly status: EvidenceStatus;
	readonly contentHash: string;
}

/**
 * Emitted as run-manifest.json beside the sidecars after a clean run. Binds
 * one runId to the ledger (referencePin + SHA-256 of its canonical JSON) and
 * the per-row evidence collected during the run. Individual sidecars carry no
 * status; the manifest is the only status-bearing artifact.
 */
export interface RunManifest {
	readonly schema: typeof RUN_MANIFEST_SCHEMA;
	readonly runId: string;
	readonly referencePin: string;
	readonly ledgerHash: string;
	readonly rowCount: number;
	readonly presentCount: number;
	readonly entries: readonly RunManifestEntry[];
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

export function sha256(data: string): string {
	return createHash("sha256").update(data, "utf8").digest("hex");
}

export function isEvidenceClass(value: unknown): value is EvidenceClass {
	return typeof value === "string" && (EVIDENCE_CLASSES as readonly string[]).includes(value);
}

/** Assert a param exists and is a string; return it or a problem. */
function requireString(
	row: LedgerRow,
	key: string,
): { ok: true; value: string } | { ok: false; problem: string } {
	const v = row.params[key];
	if (typeof v !== "string" || v.length === 0) {
		return { ok: false, problem: `[${row.id}] missing required param: ${key}` };
	}
	return { ok: true, value: v };
}

/** Assert a param exists and is a number; return it or a problem. */
function requireNumber(
	row: LedgerRow,
	key: string,
): { ok: true; value: number } | { ok: false; problem: string } {
	const v = row.params[key];
	if (typeof v !== "number" || !Number.isFinite(v)) {
		return { ok: false, problem: `[${row.id}] missing required param: ${key}` };
	}
	return { ok: true, value: v };
}

function makeSidecar(row: LedgerRow, contentHash: string, runId: string): Sidecar {
	return {
		rowId: row.id,
		contentHash,
		toolVersion: TOOL_VERSION,
		runId,
	};
}

function okResult(row: LedgerRow, contentHash: string, runId: string): RunnerResult {
	return { ok: true, sidecar: makeSidecar(row, contentHash, runId), problems: [] };
}

function failResult(row: LedgerRow, runId: string, ...problems: string[]): RunnerResult {
	return {
		ok: false,
		sidecar: makeSidecar(row, sha256(problems.join("\0")), runId),
		problems,
	};
}

function readFile(root: string, relPath: string): string | null {
	const abs = join(root, relPath);
	if (!existsSync(abs)) return null;
	return readFileSync(abs, "utf8");
}

// ---------------------------------------------------------------------------
// Runner: version-pin
// ---------------------------------------------------------------------------

/**
 * Verify a version constant at a source path matches the expected value.
 * params: { label: string, expected: string, source: string }
 */
export function runVersionPin(row: LedgerRow, root: string, runId: string): RunnerResult {
	const label = requireString(row, "label");
	if (!label.ok) return failResult(row, runId, label.problem);
	const expected = requireString(row, "expected");
	if (!expected.ok) return failResult(row, runId, expected.problem);
	const source = requireString(row, "source");
	if (!source.ok) return failResult(row, runId, source.problem);

	const content = readFile(root, source.value);
	if (content === null) {
		return failResult(row, runId, `[${row.id}] source file not found: ${source.value}`);
	}

	// Match `export const LABEL = value` or `const LABEL = value` (string or numeric)
	const re = new RegExp(
		`(?:export\\s+)?(?:const|let|var)\\s+${escapeRegex(label.value)}\\s*=\\s*["']?([A-Za-z0-9_.+-]+)["']?`,
	);
	const m = content.match(re);
	if (!m || m[1] === undefined) {
		return failResult(row, runId, `[${row.id}] label ${label.value} not found in ${source.value}`);
	}
	const actual = m[1];
	if (actual !== expected.value) {
		return failResult(
			row,
			runId,
			`[${row.id}] version mismatch for ${label.value}: expected ${expected.value}, got ${actual}`,
		);
	}
	return okResult(row, sha256(expected.value), runId);
}

// ---------------------------------------------------------------------------
// Runner: generated-block
// ---------------------------------------------------------------------------

/**
 * Verify a generated artifact is traced to its generator source.
 * params: { generator: string, artifact: string }
 */
export function runGeneratedBlock(row: LedgerRow, root: string, runId: string): RunnerResult {
	const generator = requireString(row, "generator");
	if (!generator.ok) return failResult(row, runId, generator.problem);
	const artifact = requireString(row, "artifact");
	if (!artifact.ok) return failResult(row, runId, artifact.problem);

	const genContent = readFile(root, generator.value);
	if (genContent === null) {
		return failResult(row, runId, `[${row.id}] generator source not found: ${generator.value}`);
	}
	return okResult(row, sha256(genContent), runId);
}

// ---------------------------------------------------------------------------
// Runner: fenced-compile
// ---------------------------------------------------------------------------

/**
 * Verify a fenced code block exists in a docs topic (path-registered).
 * params: { topic: string, fenceMarker: string }
 */
export function runFencedCompile(row: LedgerRow, root: string, runId: string): RunnerResult {
	const topic = requireString(row, "topic");
	if (!topic.ok) return failResult(row, runId, topic.problem);
	const fenceMarker = requireString(row, "fenceMarker");
	if (!fenceMarker.ok) return failResult(row, runId, fenceMarker.problem);

	const content = readFile(root, topic.value);
	if (content === null) {
		return failResult(row, runId, `[${row.id}] topic file not found: ${topic.value}`);
	}
	// Look for a fenced block annotated with the fenceMarker comment
	const fenceRe = new RegExp(
		"```[a-zA-Z]*\\s*[\\s\\S]*?" + escapeRegex(fenceMarker.value) + "[\\s\\S]*?```",
	);
	if (!fenceRe.test(content)) {
		return failResult(
			row,
			runId,
			`[${row.id}] fence marker ${fenceMarker.value} not found in ${topic.value}`,
		);
	}
	return okResult(row, sha256(content), runId);
}

// ---------------------------------------------------------------------------
// Runner: transcript-claim
// ---------------------------------------------------------------------------

/**
 * Verify a CLI help source contains a claimed string.
 * params: { source: string, claim: string }
 */
export function runTranscriptClaim(row: LedgerRow, root: string, runId: string): RunnerResult {
	const source = requireString(row, "source");
	if (!source.ok) return failResult(row, runId, source.problem);
	const claim = requireString(row, "claim");
	if (!claim.ok) return failResult(row, runId, claim.problem);

	const content = readFile(root, source.value);
	if (content === null) {
		return failResult(row, runId, `[${row.id}] source file not found: ${source.value}`);
	}
	if (!content.includes(claim.value)) {
		return failResult(
			row,
			runId,
			`[${row.id}] claim "${claim.value}" not found in ${source.value}`,
		);
	}
	return okResult(row, sha256(content), runId);
}

// ---------------------------------------------------------------------------
// Runner: matrix-count
// ---------------------------------------------------------------------------

/**
 * Verify a matrix file has the expected item count.
 * params: { source: string, expectedCount: number, countMethod: "json-array" | "regex", countKey: string }
 */
export function runMatrixCount(row: LedgerRow, root: string, runId: string): RunnerResult {
	const source = requireString(row, "source");
	if (!source.ok) return failResult(row, runId, source.problem);
	const expectedCount = requireNumber(row, "expectedCount");
	if (!expectedCount.ok) return failResult(row, runId, expectedCount.problem);
	const countMethod = requireString(row, "countMethod");
	if (!countMethod.ok) return failResult(row, runId, countMethod.problem);
	const countKey = requireString(row, "countKey");
	if (!countKey.ok) return failResult(row, runId, countKey.problem);

	const content = readFile(root, source.value);
	if (content === null) {
		return failResult(row, runId, `[${row.id}] source file not found: ${source.value}`);
	}

	let actualCount: number;
	if (countMethod.value === "json-array") {
		let parsed: unknown;
		try {
			parsed = JSON.parse(content);
		} catch {
			return failResult(row, runId, `[${row.id}] source is not valid JSON: ${source.value}`);
		}
		const arr = (parsed as Record<string, unknown>)[countKey.value];
		if (!Array.isArray(arr)) {
			return failResult(
				row,
				runId,
				`[${row.id}] countKey ${countKey.value} is not an array in ${source.value}`,
			);
		}
		actualCount = arr.length;
	} else if (countMethod.value === "regex") {
		// Count quoted string entries in a TypeScript array named countKey
		const arrRe = new RegExp(
			`(?:export\\s+)?(?:const|let|var)\\s+${escapeRegex(countKey.value)}\\s*=\\s*\\[([\\s\\S]*?)\\]`,
		);
		const arrMatch = content.match(arrRe);
		if (!arrMatch || arrMatch[1] === undefined) {
			return failResult(
				row,
				runId,
				`[${row.id}] array ${countKey.value} not found in ${source.value}`,
			);
		}
		const entries = arrMatch[1].match(/"[^"]+"/g);
		actualCount = entries ? entries.length : 0;
	} else {
		return failResult(
			row,
			runId,
			`[${row.id}] unknown countMethod: ${countMethod.value}`,
		);
	}

	if (actualCount !== expectedCount.value) {
		return failResult(
			row,
			runId,
			`[${row.id}] matrix count mismatch: expected ${expectedCount.value}, got ${actualCount}`,
		);
	}
	return okResult(row, sha256(String(actualCount)), runId);
}

// ---------------------------------------------------------------------------
// Runner: review-only-prose
// ---------------------------------------------------------------------------

/**
 * Verify a prose surface file exists and hash its content.
 * params: { source: string }
 */
export function runReviewOnlyProse(row: LedgerRow, root: string, runId: string): RunnerResult {
	const source = requireString(row, "source");
	if (!source.ok) return failResult(row, runId, source.problem);

	const content = readFile(root, source.value);
	if (content === null) {
		return failResult(row, runId, `[${row.id}] prose source not found: ${source.value}`);
	}
	return okResult(row, sha256(content), runId);
}

// ---------------------------------------------------------------------------
// Runner: changelog-unreleased
// ---------------------------------------------------------------------------

/**
 * Verify a CHANGELOG has a ## [Unreleased] section.
 * params: { source: string }
 */
export function runChangelogUnreleased(row: LedgerRow, root: string, runId: string): RunnerResult {
	const source = requireString(row, "source");
	if (!source.ok) return failResult(row, runId, source.problem);

	const content = readFile(root, source.value);
	if (content === null) {
		return failResult(row, runId, `[${row.id}] changelog not found: ${source.value}`);
	}
	const unreleasedRe = /^## \[Unreleased\]/m;
	if (!unreleasedRe.test(content)) {
		return failResult(
			row,
			runId,
			`[${row.id}] ## [Unreleased] section not found in ${source.value}`,
		);
	}
	// Hash the unreleased section content (from the header to the next ## section)
	const idx = content.search(unreleasedRe);
	const rest = content.slice(idx);
	const nextSection = rest.slice(3).search(/^## /m);
	const section = nextSection === -1 ? rest : rest.slice(0, nextSection + 3);
	// Check for evidence-free Unreleased entries (DOC-G2 adversarial hardening)
	const evidenceProblems = checkUnreleasedEntriesHaveEvidence(row.id, section);
	if (evidenceProblems.length > 0) {
		return failResult(row, runId, ...evidenceProblems);
	}

	return okResult(row, sha256(section), runId);
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/**
 * Run the appropriate evidence-class runner for a ledger row.
 * Returns a RunnerResult with a fresh sidecar and any problems.
 */
export function runEvidence(row: LedgerRow, root: string, runId: string): RunnerResult {
	switch (row.class) {
		case "version-pin":
			return runVersionPin(row, root, runId);
		case "generated-block":
			return runGeneratedBlock(row, root, runId);
		case "fenced-compile":
			return runFencedCompile(row, root, runId);
		case "transcript-claim":
			return runTranscriptClaim(row, root, runId);
		case "matrix-count":
			return runMatrixCount(row, root, runId);
		case "review-only-prose":
			return runReviewOnlyProse(row, root, runId);
		case "changelog-unreleased":
			return runChangelogUnreleased(row, root, runId);
		default:
			return failResult(row, runId, `[${row.id}] unknown evidence class: ${row.class}`);
	}
}

// ---------------------------------------------------------------------------
// Staleness checking
// ---------------------------------------------------------------------------

export interface StalenessResult {
	readonly stale: boolean;
	readonly reasons: readonly string[];
}

/**
 * Compare a prior sidecar against a fresh runner result.
 * Staleness fails the run: contentHash mismatch, toolVersion mismatch,
 * or runId older than the re-proof interval.
 */
export function checkStaleness(
	prior: Sidecar,
	fresh: Sidecar,
	reproofIntervalMs: number,
): StalenessResult {
	const reasons: string[] = [];

	if (prior.contentHash !== fresh.contentHash) {
		reasons.push(
			`contentHash mismatch for ${prior.rowId}: expected ${fresh.contentHash}, sidecar has ${prior.contentHash}`,
		);
	}
	if (prior.toolVersion !== fresh.toolVersion) {
		reasons.push(
			`toolVersion mismatch for ${prior.rowId}: expected ${fresh.toolVersion}, sidecar has ${prior.toolVersion}`,
		);
	}
	const priorTime = Date.parse(prior.runId);
	const freshTime = Date.parse(fresh.runId);
	if (Number.isNaN(priorTime)) {
		reasons.push(`prior sidecar runId is not a valid timestamp: ${prior.runId}`);
	} else if (Number.isNaN(freshTime)) {
		// Fresh runId is not a timestamp — skip the age check
	} else {
		const ageMs = freshTime - priorTime;
		if (ageMs > reproofIntervalMs) {
			reasons.push(
				`runId for ${prior.rowId} is ${Math.round(ageMs / (24 * 60 * 60 * 1000))} days old (max ${Math.round(reproofIntervalMs / (24 * 60 * 60 * 1000))} days)`,
			);
		}
	}

	return { stale: reasons.length > 0, reasons };
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

function escapeRegex(s: string): string {
	return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// ---------------------------------------------------------------------------
// DOC-G2 adversarial hardening: evidence-free Unreleased entry detection
// ---------------------------------------------------------------------------

/** Patterns that count as commit evidence in a CHANGELOG bullet entry. */
const COMMIT_SHA_RE = /\b[0-9a-f]{7,40}\b/i;
const ISSUE_REF_RE = /\[#\d+\]|\(#\d+\)/;
const URL_RE = /https?:\/\/./;

/**
 * Check that every bullet entry under ## [Unreleased] carries commit evidence
 * (a commit SHA, a PR/issue reference like [#NNN], or a URL).
 * Returns a list of problem strings for entries that lack all three.
 */
export function checkUnreleasedEntriesHaveEvidence(
	rowId: string,
	section: string,
): string[] {
	const problems: string[] = [];
	for (const line of section.split("\n")) {
		if (!/^\s*[-*]\s+\S/.test(line)) continue;
		if (COMMIT_SHA_RE.test(line) || ISSUE_REF_RE.test(line) || URL_RE.test(line)) continue;
		problems.push(
			`[${rowId}] evidence-free Unreleased entry (no commit SHA, PR/issue ref, or URL): ${line.trim()}`,
		);
	}
	return problems;
}

// ---------------------------------------------------------------------------
// DOC-G2 adversarial hardening: disguised example-product import detection
// ---------------------------------------------------------------------------

/** Directories whose .ts files are scanned for disguised example-product imports. */
export const EXAMPLE_PRODUCT_SCAN_DIRS = [
	"scripts/tests",
	"scripts/verification",
] as const;

/** Path segment that identifies the example-product (reference corpus) tree. */
export const EXAMPLE_PRODUCT_MARKER = ".references/pi/";

/**
 * Scan test and verification .ts files for value import statements referencing
 * the example-product tree (.references/pi/).  A disguised import accretes
 * example-product behavior into the evidence program and is a drift class
 * the checker must catch.  Type-only imports are skipped (erased at runtime).
 *
 * Returns a list of findings: "file:line — disguised example-product import".
 */
export function scanForExampleProductImports(root: string): string[] {
	const findings: string[] = [];
	for (const dir of EXAMPLE_PRODUCT_SCAN_DIRS) {
		scanDirForExampleProductImports(join(root, dir), findings);
	}
	return findings;
}

function scanDirForExampleProductImports(dir: string, findings: string[]): void {
	if (!existsSync(dir)) return;
	for (const entry of readdirSync(dir)) {
		const fullPath = join(dir, entry);
		let stat;
		try {
			stat = statSync(fullPath);
		} catch {
			continue;
		}
		if (stat.isDirectory()) {
			scanDirForExampleProductImports(fullPath, findings);
		} else if (entry.endsWith(".ts")) {
			const content = readFileSync(fullPath, "utf8");
			const lines = content.split("\n");
			for (let i = 0; i < lines.length; i++) {
				const line = lines[i];
				if (line === undefined) continue;
				// Flag value imports/re-exports (not type-only) referencing the example-product tree.
				// Type-only imports are erased at runtime and do not accrete behavior.
				const isValueImport = /^\s*import\s(?!type\s)/.test(line);
				const isValueReExport = /^\s*export\s+(?!type\s).*\bfrom\s/.test(line);
				if (line.includes(EXAMPLE_PRODUCT_MARKER) && (isValueImport || isValueReExport)) {
					findings.push(`${fullPath}:${i + 1} — disguised example-product import`);
				}
			}
		}
	}
}
