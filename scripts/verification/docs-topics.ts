#!/usr/bin/env bun
/**
 * DOC-C user-doc corpus port checker (issue #137).
 *
 * Owns the ported-topic half of the doc-evidence program:
 *
 *   - enumerates the reference corpus at the settled pin (computed at checker
 *     time from `.references/pi`, never a hardcoded count),
 *   - gates `docs/index.md` shipped/pending lists against that enumeration,
 *   - proves every shipped topic carries per-topic evidence sidecars whose
 *     runners executed in the current run,
 *   - binds fenced examples to the fenced-compile ledger (cross-ledger
 *     bijection) and snapshot-derived fences to the transcript artifacts they
 *     quote verbatim,
 *   - enforces the inline-import closed set (pi, pi_ai, pi_agent for Rust
 *     fences; the extension-host / pi-tui-protocol entrypoints for TS fences),
 *   - locks the five terminal-visual topics out of registration until
 *     TUI-CLOSE (#82) / EXT-25 land.
 *
 * This module never edits the reference tree, the terminal transcript schema,
 * the verification harnesses, crates, or packages.
 */

import { existsSync, mkdirSync, readFileSync, readdirSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

/** Manifest schema for docs/evidence/<topic>.json. */
export const MANIFEST_SCHEMA = "pi.docs.topic-evidence.v1";

/** Reference corpus root (read-only, settled pin). */
export const CORPUS_ROOT = ".references/pi";

/** Coding-agent reference docs directory (relative to repo root). */
export const CODING_AGENT_DOCS = `${CORPUS_ROOT}/packages/coding-agent/docs`;

/** Agent-core reference docs directory (relative to repo root). */
export const AGENT_DOCS = `${CORPUS_ROOT}/packages/agent/docs`;

/** Where this module writes its own transcript snapshots. */
export const TRANSCRIPT_DIR = "target/verification/docs-topics";

/**
 * Terminal-visual topics. Their sidecars must carry the terminal transcript
 * schema's toolVersion and grades consistent with landed TUI-CLOSE / EXT-25
 * evidence. They cannot be registered before those land.
 */
export const TERMINAL_VISUAL_TOPICS = [
	"terminal-setup",
	"termux",
	"tmux",
	"tui",
	"windows",
] as const;

/**
 * Gate flag: flips to true only when TUI-CLOSE (#82) and EXT-25 have landed
 * with consolidated terminal evidence. While false, any attempt to ship or
 * register a terminal-visual topic fails the checker.
 */
export const TERMINAL_TOPIC_GATE_UNLOCKED = false;

export const TERMINAL_TOPIC_GATE_REASON =
	"TUI-CLOSE (#82) / EXT-25 have not landed; terminal-visual topics carry no evidence beyond the landed transcript schema";

/** Allowed `use` roots inside rust fences of ported topics. */
export const ALLOWED_RUST_USE_ROOTS = ["pi", "pi_ai", "pi_agent", "std"] as const;

/** Allowed import specifiers inside ts/tsx fences of ported topics. */
export const ALLOWED_TS_IMPORT_SPECIFIERS = [
	"pi",
	"pi-ai",
	"pi-agent",
	".",
	"./sanitize",
	"./protocol",
] as const;

/** Closed set of transcript kinds a manifest may bind. */
export const TRANSCRIPT_KINDS = [
	"cli-help",
	"release-flags",
	"e2e-smoke",
	"rpc-parity",
	"session-interop",
] as const;

/** Closed set of pending reasons. A pending item must name its blocker. */
export const PENDING_REASONS = [
	"TUI-CLOSE",
	"EXT-25",
	"PAR-CLOSE",
	"XC-CLOSE",
	"DOC-D",
	"unported-feature",
] as const;

/** Fence marker prefix used inside ported-topic code fences. */
export const FENCE_MARKER_PREFIX = "doc-c:fence=";

/** Source annotation appended to a fence marker for snapshot-derived fences. */
export const FENCE_SOURCE_PREFIX = " source=";

/** Index list markers parsed by checkDocsTopics. */
export const INDEX_SHIPPED_BEGIN = "<!-- doc-c:index-shipped-begin -->";
export const INDEX_SHIPPED_END = "<!-- doc-c:index-shipped-end -->";
export const INDEX_PENDING_BEGIN = "<!-- doc-c:index-pending-begin -->";
export const INDEX_PENDING_END = "<!-- doc-c:index-pending-end -->";

/** CLI snapshots captured fresh on every checker run. */
export const CLI_SNAPSHOT_SPECS = [
	{ name: "pi--help.txt", argv: ["--help"] },
	{ name: "pi--version.txt", argv: ["--version"] },
	{ name: "pi-package--help.txt", argv: ["package", "--help"] },
	{ name: "pi-config--help.txt", argv: ["config", "--help"] },
] as const;

/** Release binary invoked for CLI snapshots. */
export const RELEASE_BINARY = "target/release/pi";

/** Stable mirror of the latest e2e-smoke result (normalized to repo-relative paths). */
export const E2E_RESULT_MIRROR = `${TRANSCRIPT_DIR}/harness/e2e-result.json`;

/** Stable mirror of the e2e run's first session JSONL. */
export const E2E_SESSION_MIRROR = `${TRANSCRIPT_DIR}/harness/e2e-session.jsonl`;

/** Stable mirror of the session-interop v3/basic fork output. */
export const SESSION_INTEROP_FORK_MIRROR = `${TRANSCRIPT_DIR}/harness/session-interop-fork.jsonl`;

/** Marker file naming the latest e2e run directory (stable path, harness-owned). */
export const E2E_LATEST_RUN = "target/verification/e2e/latest-run.txt";

// ---------------------------------------------------------------------------
// Corpus enumeration (computed at checker time, never hardcoded)
// ---------------------------------------------------------------------------

export interface CorpusTopic {
	readonly slug: string;
	readonly referencePath: string;
	readonly corpus: "coding-agent" | "agent-core";
}

function listCorpusDir(root: string, dir: string, corpus: CorpusTopic["corpus"]): CorpusTopic[] {
	const abs = resolve(root, dir);
	if (!existsSync(abs)) {
		throw new Error(`doc-c: reference corpus directory missing: ${dir}`);
	}
	const topics: CorpusTopic[] = [];
	for (const entry of readdirSync(abs)) {
		if (!entry.endsWith(".md") || entry === "index.md") continue;
		topics.push({ slug: entry.slice(0, -3), referencePath: `${dir}/${entry}`, corpus });
	}
	topics.sort((a, b) => a.slug.localeCompare(b.slug));
	return topics;
}

/** Enumerate every corpus topic at the pinned reference tree. */
export function enumerateCorpusTopics(root: string): readonly CorpusTopic[] {
	return [...listCorpusDir(root, CODING_AGENT_DOCS, "coding-agent"), ...listCorpusDir(root, AGENT_DOCS, "agent-core")];
}

/** Terminal-visual topics that exist in the enumerated corpus. */
export function terminalTopicsInCorpus(corpus: readonly CorpusTopic[]): readonly CorpusTopic[] {
	const names = new Set<string>(TERMINAL_VISUAL_TOPICS);
	return corpus.filter((t) => names.has(t.slug));
}

// ---------------------------------------------------------------------------
// Manifest I/O
// ---------------------------------------------------------------------------

export interface ManifestTranscript {
	readonly kind: string;
	readonly source: string;
	readonly producer: string;
}

export interface ManifestClaim {
	readonly rowId: string;
	readonly source: string;
	readonly claim: string;
}

export interface ManifestPending {
	readonly item: string;
	readonly reason: string;
}

export interface TopicManifest {
	readonly schema: string;
	readonly topic: string;
	readonly doc: string;
	readonly referenceSource: string;
	readonly transcripts: readonly ManifestTranscript[];
	readonly claims: readonly ManifestClaim[];
	readonly pending: readonly ManifestPending[];
}

export function manifestPath(topic: string): string {
	return `docs/evidence/${topic}.json`;
}

export function topicDocPath(topic: string): string {
	return `docs/${topic}.md`;
}

// ---------------------------------------------------------------------------
// Fresh transcript capture (executed in the current run)
// ---------------------------------------------------------------------------

export interface CapturedSnapshot {
	readonly name: string;
	readonly source: string;
	readonly ok: boolean;
	readonly stdout: string;
}

function captureProcess(
	root: string,
	argv: readonly string[],
	envHome: string,
): { ok: boolean; stdout: string } {
	const result = spawnSync(resolve(root, argv[0] ?? "pi"), argv.slice(1), {
		encoding: "utf8",
		timeout: 30_000,
		cwd: root,
		env: {
			...process.env,
			HOME: envHome,
			PI_CODING_AGENT_DIR: join(envHome, "agent"),
			PI_OFFLINE: "1",
		},
	});
	return { ok: result.status === 0, stdout: `${result.stdout ?? ""}${result.stderr ?? ""}` };
}

/** Execute the release binary and record --help/--version snapshots. */
export function captureCliHelpSnapshots(root: string): readonly CapturedSnapshot[] {
	const outDir = resolve(root, TRANSCRIPT_DIR, "cli-help");
	mkdirSync(outDir, { recursive: true });
	const envHome = join(outDir, ".home");
	mkdirSync(envHome, { recursive: true });
	const captured: CapturedSnapshot[] = [];
	for (const spec of CLI_SNAPSHOT_SPECS) {
		const run = captureProcess(root, [RELEASE_BINARY, ...spec.argv], envHome);
		const source = `${TRANSCRIPT_DIR}/cli-help/${spec.name}`;
		writeFileSync(join(outDir, spec.name), run.stdout, "utf8");
		captured.push({ name: spec.name, source, ok: run.ok, stdout: run.stdout });
	}
	return captured;
}

/**
 * Execute the release script's parseReleaseArgs against every documented flag
 * and record the accepted flag set. The probe imports the real module; it
 * never re-states the flag set by hand.
 */
export function captureReleaseFlagSnapshot(root: string): CapturedSnapshot {
	const outDir = resolve(root, TRANSCRIPT_DIR);
	mkdirSync(outDir, { recursive: true });
	const flagCases = [...readFileSync(resolve(root, "scripts/release/args.ts"), "utf8").matchAll(/case\s+"(-{1,2}[a-z][a-z0-9-]*)"/g)]
		.map((m) => m[1] ?? "")
		.filter((f) => f !== "-h" && f !== "--help");
	const flags = [...new Set(flagCases)].sort();
	const probe = `
import { parseReleaseArgs, ArgvHelpRequested } from "./scripts/release/args.ts";
const valueFlags = new Set(["--target", "--out", "--out-dir", "--runtime-cache", "--source-date-epoch"]);
const flags = ${JSON.stringify(flags)};
const lines = [];
for (const flag of flags) {
	const argv = valueFlags.has(flag) ? [flag, flag === "--source-date-epoch" ? "0" : "probe-value"] : [flag, "--target", "x86_64-unknown-linux-gnu"];
	try {
		parseReleaseArgs(argv);
		lines.push(flag + " accepted");
	} catch (e) {
		lines.push(flag + " REJECTED: " + (e instanceof Error ? e.constructor.name : String(e)));
	}
}
for (const probeArg of ["--target", "--help", "-h", "--bogus"]) {
	try {
		parseReleaseArgs(probeArg === "--bogus" ? ["--bogus"] : [probeArg, "x86_64-unknown-linux-gnu"]);
		lines.push(probeArg + " accepted" + (probeArg === "--help" || probeArg === "-h" ? " (help request)" : ""));
	} catch (e) {
		lines.push(probeArg + " " + (e instanceof ArgvHelpRequested ? "accepted (help request)" : e instanceof Error && e.constructor.name === "UnknownArgError" ? "rejected: UnknownArgError" : "REJECTED: " + (e instanceof Error ? e.constructor.name : String(e))));
	}
}
console.log(lines.join("\\n"));
`;
	const result = spawnSync("bun", ["-e", probe], { encoding: "utf8", timeout: 60_000, cwd: root });
	const stdout = `${result.stdout ?? ""}${result.stderr ?? ""}`;
	const source = `${TRANSCRIPT_DIR}/release-flags.txt`;
	writeFileSync(resolve(root, source), stdout, "utf8");
	return { name: "release-flags.txt", source, ok: result.status === 0, stdout };
}

/**
 * Deterministic normalization applied to mirrored harness outputs so the
 * mirrors (and every fence that quotes them) are byte-stable across machines
 * and runs: absolute repo paths collapse, random run-directory tokens,
 * session-file names, UUIDs, minted 8-hex entry ids, and wall-clock
 * timestamps are redacted to fixed placeholders.
 */
export function normalizeHarnessText(text: string, rootAbs: string): string {
	return text
		.split(rootAbs)
		.join("")
		.replace(/run-[A-Za-z0-9_-]+/g, "run-<latest>")
		.replace(/\d{4}-\d{2}-\d{2}T[0-9:.\\-]+Z(?:_[0-9a-f-]+)?/g, "<ts>")
		.replace(/\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/g, "<uuid>")
		.replace(/"id"\s*:\s*"[0-9a-f]{8}"/g, '"id": "<id>"')
		.replace(/"([a-zA-Z]*)Id"\s*:\s*"[0-9a-f]{8}"/g, '"$1Id": "<id>"')
		.replace(/"([a-zA-Z]*)Instance"\s*:\s*"[^"]*"/g, '"$1Instance": "<instance>"')
		.replace(/"instance"\s*:\s*"[^"]*"/g, '"instance": "<instance>"')
		.replace(/\b\d{1,10}:\d{12,}\b/g, "<instance>")
		.replace(/"bunVersion"\s*:\s*"[^"]*"/g, '"bunVersion": "<bun>"')
		.replace(/"sha256[A-Za-z]*"\s*:\s*"[0-9a-f]{12,}"/g, '"sha256": "<sha256>"')
		.replace(/"timestamp"\s*:\s*\d{12,}/g, '"timestamp": <ms>')
		.replace(/"timestamp"\s*:\s*"20\d\d-[^"]*"/g, '"timestamp": "<ts>"')
		.replace(/"startedAt"\s*:\s*"[^"]*"/g, '"startedAt": "<ts>"')
		.replace(/"finishedAt"\s*:\s*"[^"]*"/g, '"finishedAt": "<ts>"');
}

/**
 * Mirror the latest e2e-smoke and session-interop outputs into stable,
 * machine-independent, run-independent paths under the DOC-C transcript
 * directory. The mirrors keep the upstream mtime so freshness reflects
 * harness execution time. Returns the mirrors written; missing upstream
 * artifacts yield nulls.
 */
export function mirrorHarnessArtifacts(root: string): {
	readonly e2eResult: string | null;
	readonly e2eSession: string | null;
	readonly sessionInteropContinued: string | null;
	readonly sessionInteropFork: string | null;
} {
	const outDir = resolve(root, TRANSCRIPT_DIR, "harness");
	mkdirSync(outDir, { recursive: true });
	const rootAbs = resolve(root);

	const writeNormalized = (from: string, to: string): boolean => {
		if (!existsSync(from)) return false;
		const normalized = normalizeHarnessText(readFileSync(from, "utf8"), rootAbs);
		writeFileSync(resolve(root, to), normalized, "utf8");
		const mtime = statSync(from).mtime;
		utimesSync(resolve(root, to), mtime, mtime);
		return true;
	};

	let e2eResult: string | null = null;
	let e2eSession: string | null = null;
	const latestPath = resolve(root, E2E_LATEST_RUN);
	if (existsSync(latestPath)) {
		const runName = readFileSync(latestPath, "utf8").trim();
		const runDir = resolve(root, "target/verification/e2e", runName);
		if (writeNormalized(join(runDir, "result.json"), E2E_RESULT_MIRROR)) {
			e2eResult = E2E_RESULT_MIRROR;
		}
		const sessionsDir = join(runDir, "sessions");
		if (existsSync(sessionsDir)) {
			const candidates = readdirSync(sessionsDir).filter((f) => f.endsWith(".jsonl"));
			const chosen = pickDeterministic(
				candidates.map((f) => join(sessionsDir, f)),
				(content) => normalizeHarnessText(content, rootAbs),
			);
			if (chosen !== null) {
				if (writeNormalized(chosen, E2E_SESSION_MIRROR)) {
					e2eSession = E2E_SESSION_MIRROR;
				}
			}
		}
	}

	const continuedTargets = [
		{ upstream: "target/verification/session-interop/v1/linear-with-compaction/continued.jsonl", mirror: `${TRANSCRIPT_DIR}/harness/session-interop-v1-continued.jsonl` },
		{ upstream: "target/verification/session-interop/v3/basic/continued.jsonl", mirror: `${TRANSCRIPT_DIR}/harness/session-interop-v3-continued.jsonl` },
	];
	let sessionInteropContinued: string | null = null;
	for (const t of continuedTargets) {
		if (writeNormalized(resolve(root, t.upstream), t.mirror)) {
			sessionInteropContinued = t.mirror;
		}
	}

	let sessionInteropFork: string | null = null;
	const basicDir = resolve(root, "target/verification/session-interop/v3/basic");
	if (existsSync(basicDir)) {
		const candidates = readdirSync(basicDir)
			.filter((f) => f.endsWith(".jsonl") && f !== "continued.jsonl")
			.map((f) => join(basicDir, f));
		const chosen = pickDeterministic(candidates, (content) => normalizeHarnessText(content, rootAbs));
		if (chosen !== null) {
			if (writeNormalized(chosen, SESSION_INTEROP_FORK_MIRROR)) {
				sessionInteropFork = SESSION_INTEROP_FORK_MIRROR;
			}
		}
	}

	return { e2eResult, e2eSession, sessionInteropContinued, sessionInteropFork };
}

/**
 * Pick one file deterministically: order candidates by their normalized
 * content (then by path), so name-level randomness (same-millisecond ids,
 * random uuid suffixes) cannot flip the selection between runs.
 */
function pickDeterministic(paths: readonly string[], normalize: (content: string) => string): string | null {
	const keyed = paths
		.map((p) => ({ p, key: normalize(readFileSync(p, "utf8")) }))
		.sort((a, b) => (a.key === b.key ? a.p.localeCompare(b.p) : a.key < b.key ? -1 : 1));
	return keyed[0]?.p ?? null;
}

// ---------------------------------------------------------------------------
// Fence extraction and import scanning
// ---------------------------------------------------------------------------

export interface TopicFence {
	readonly markerId: string;
	readonly source: string | null;
	readonly language: string;
	readonly body: readonly string[];
}

const FENCE_OPEN_RE = /^(`{3,}|~{3,})([^\n]*)$/;

export function extractTopicFences(content: string): readonly TopicFence[] {
	const lines = content.split("\n");
	const fences: TopicFence[] = [];
	let inFence = false;
	let fenceChar = "";
	let language = "";
	let body: string[] = [];
	for (const line of lines) {
		if (!inFence) {
			const open = FENCE_OPEN_RE.exec(line);
			if (open) {
				inFence = true;
				fenceChar = open[1]?.[0] ?? "`";
				language = (open[2] ?? "").trim().split(",")[0]?.trim() ?? "";
				body = [];
			}
			continue;
		}
		const closeRe = fenceChar === "~" ? /^~{3,}\s*$/ : /^`{3,}\s*$/;
		if (closeRe.test(line)) {
			inFence = false;
			const text = body.join("\n");
			const markerIdx = text.indexOf(FENCE_MARKER_PREFIX);
			if (markerIdx >= 0) {
				const rest = text.slice(markerIdx + FENCE_MARKER_PREFIX.length);
				const id = rest.split(/[\s<]/)[0] ?? "";
				let source: string | null = null;
				const srcIdx = rest.indexOf(FENCE_SOURCE_PREFIX);
				if (srcIdx >= 0) {
					source = rest
						.slice(srcIdx + FENCE_SOURCE_PREFIX.length)
						.split(/\s|<|-->/)[0]
						?.trimEnd() ?? null;
				}
				fences.push({ markerId: id, source, language, body });
			} else if (text.trim().length > 0) {
				fences.push({ markerId: "", source: null, language, body });
			}
			continue;
		}
		body.push(line);
	}
	return fences;
}

const TS_IMPORT_RE = /(?:^|\n)\s*(?:import|export)\s[^;]*?from\s+["']([^"']+)["']/g;
const TS_DYNAMIC_RE = /\b(?:require|import)\s*\(\s*["']([^"']+)["']\s*\)/g;
const RUST_USE_RE = /(?:^|\n)\s*use\s+([a-zA-Z_][a-zA-Z0-9_]*)/g;
const RUST_FQ_RE = /(?<![a-zA-Z0-9_:])([a-z_][a-z0-9_]*)::/g;
const RUST_PATH_SELF = new Set(["crate", "self", "super", "str", "f32", "f64", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "usize", "isize"]);
const TS_SIDE_EFFECT_RE = /(?:^|\n)\s*import\s+["']([^"']+)["']/g;
/**
 * Scan a fence body for import specifiers outside the closed entrypoint set.
 * A fence whose source is a verbatim exhibit of committed repo code
 * (scripts/ or crates/) may use that code's own imports, including node:*.
 */
export function scanFenceImports(fence: TopicFence): readonly string[] {
	const body = fence.body.filter((l) => !l.includes(FENCE_MARKER_PREFIX)).join("\n");
	const bad: string[] = [];
	const exhibit = fence.source !== null && (fence.source.startsWith("scripts/") || fence.source.startsWith("crates/"));
	if (fence.language === "rust") {
		for (const m of body.matchAll(RUST_USE_RE)) {
			const root = m[1] ?? "";
			if (exhibit || !(ALLOWED_RUST_USE_ROOTS as readonly string[]).includes(root)) {
				if (!exhibit) bad.push(root);
			}
		}
		for (const m of body.matchAll(RUST_FQ_RE)) {
			const root = m[1] ?? "";
			if (exhibit) continue;
			if (
				RUST_PATH_SELF.has(root) ||
				ALLOWED_RUST_USE_ROOTS.includes(root as (typeof ALLOWED_RUST_USE_ROOTS)[number])
			) {
				continue;
			}
			bad.push(`${root}::`);
		}
	}
	if (fence.language === "ts" || fence.language === "tsx" || fence.language === "typescript") {
		for (const m of [...body.matchAll(TS_IMPORT_RE), ...body.matchAll(TS_DYNAMIC_RE), ...body.matchAll(TS_SIDE_EFFECT_RE)]) {
			const spec = m[1] ?? "";
			const allowed =
				(ALLOWED_TS_IMPORT_SPECIFIERS as readonly string[]).includes(spec) ||
				(exhibit && (spec.startsWith("node:") || spec.startsWith("./")));
			if (!allowed) {
				bad.push(spec);
			}
		}
	}
	return bad;
}

/** Canonical transcript artifact paths per kind (closed enumeration). */
export function transcriptSourceAllowed(kind: string, source: string): boolean {
	if (kind === "cli-help") {
		return CLI_SNAPSHOT_SPECS.some((spec) => source === `${TRANSCRIPT_DIR}/cli-help/${spec.name}`);
	}
	if (kind === "release-flags") {
		return source === `${TRANSCRIPT_DIR}/release-flags.txt`;
	}
	if (kind === "e2e-smoke") {
		return source === E2E_LATEST_RUN || source === E2E_RESULT_MIRROR || source === E2E_SESSION_MIRROR;
	}
	if (kind === "session-interop") {
		return (
			source === SESSION_INTEROP_FORK_MIRROR ||
			source === `${TRANSCRIPT_DIR}/harness/session-interop-v1-continued.jsonl` ||
			source === `${TRANSCRIPT_DIR}/harness/session-interop-v3-continued.jsonl`
		);
	}
	if (kind === "rpc-parity") {
		return source.startsWith("target/verification/rpc-parity/");
	}
	return false;
}

/** The quoteable body of a snapshot-derived fence (marker comment stripped). */
export function fenceQuoteBody(fence: TopicFence): string {
	const kept = fence.body.filter((l) => !l.includes(FENCE_MARKER_PREFIX));
	// Trim leading/trailing blank lines so tables can be embedded mid-snapshot.
	let start = 0;
	let end = kept.length;
	while (start < end && kept[start]?.trim() === "") start += 1;
	while (end > start && kept[end - 1]?.trim() === "") end -= 1;
	return kept.slice(start, end).join("\n");
}

// ---------------------------------------------------------------------------
// Index parsing
// ---------------------------------------------------------------------------

export interface PendingEntry {
	readonly slug: string;
	readonly reason: string;
}

export interface ParsedIndex {
	readonly shipped: readonly string[];
	readonly pending: readonly PendingEntry[];
}

function parseListBlock(content: string, begin: string, end: string): readonly string[] {
	const beginIdx = content.indexOf(begin);
	const endIdx = content.indexOf(end);
	if (beginIdx < 0 || endIdx < 0 || endIdx < beginIdx) return [];
	return content
		.slice(beginIdx + begin.length, endIdx)
		.split("\n")
		.map((l) => l.trim())
		.filter((l) => l.startsWith("- "));
}

export function parseIndex(content: string): ParsedIndex {
	const shipped = parseListBlock(content, INDEX_SHIPPED_BEGIN, INDEX_SHIPPED_END)
		.map((l) => /\(([^)]+\.md)\)/.exec(l)?.[1]?.replace(/\.md$/, "").split("/").pop())
		.filter((s): s is string => typeof s === "string");
	const pending = parseListBlock(content, INDEX_PENDING_BEGIN, INDEX_PENDING_END)
		.map((l) => {
			const rest = l.slice(2).trim();
			const slug = /^([a-z0-9-]+)/.exec(rest)?.[1] ?? "";
			const reason = /\b([A-Za-z0-9-]+)\s*$/.exec(rest)?.[1] ?? "";
			return { slug, reason };
		})
		.filter((e) => e.slug.length > 0);
	return { shipped, pending };
}

// ---------------------------------------------------------------------------
// The DOC-C check
// ---------------------------------------------------------------------------

export interface DocsTopicsContext {
	readonly root: string;
	readonly referencePin: string;
	/** Ledger rows of the whole ledger. */
	readonly ledgerRows: readonly { id: string; owner: string; surface: string; class: string; params: Record<string, unknown> }[];
	/** Row ids that produced a fresh, non-stale sidecar in the current run. */
	readonly freshRowIds: ReadonlySet<string>;
	/** Sidecar dir (for prior-sidecar lookups). */
	readonly sidecarDir: string;
}

function terminalDocPaths(): readonly string[] {
	return TERMINAL_VISUAL_TOPICS.map((slug) => `docs/${slug}.md`);
}

/** Run every DOC-C gate; returns problems (empty = pass). */
export function checkDocsTopics(ctx: DocsTopicsContext): readonly string[] {
	const problems: string[] = [];
	const { root, referencePin } = ctx;

	// --- Corpus enumeration (computed, never hardcoded) ---
	let corpus: readonly CorpusTopic[];
	try {
		corpus = enumerateCorpusTopics(root);
	} catch (error) {
		const detail = error instanceof Error ? error.message : String(error);
		return [`[doc-c-corpus] enumeration failed: ${detail}`];
	}
	const corpusSlugs = new Set(corpus.map((t) => t.slug));

	// --- Index parse ---
	const indexPath = "docs/index.md";
	const indexContent = readFileSync(resolve(root, indexPath), "utf8");
	const index = parseIndex(indexContent);

	// --- Terminal gate ---
	const terminal = new Set(terminalTopicsInCorpus(corpus).map((t) => t.slug));
	for (const slug of index.shipped) {
		if (terminal.has(slug) && !TERMINAL_TOPIC_GATE_UNLOCKED) {
			problems.push(
				`[doc-c-terminal-gate] topic "${slug}" is terminal-visual and cannot be registered: ${TERMINAL_TOPIC_GATE_REASON}`,
			);
		}
	}
	const lockedPaths = TERMINAL_TOPIC_GATE_UNLOCKED ? [] : terminalDocPaths();
	for (const row of ctx.ledgerRows) {
		if (row.owner !== "DOC-C") continue;
		for (const locked of lockedPaths) {
			if (row.surface.includes(locked)) {
				problems.push(
					`[doc-c-terminal-gate] ledger row ${row.id} registers terminal-visual surface "${row.surface}": ${TERMINAL_TOPIC_GATE_REASON}`,
				);
			}
			const topic = typeof row.params.topic === "string" ? row.params.topic : null;
			if (topic === locked) {
				problems.push(
					`[doc-c-terminal-gate] ledger row ${row.id} registers terminal-visual topic "${topic}": ${TERMINAL_TOPIC_GATE_REASON}`,
				);
			}
		}
	}

	// --- Index list equals corpus enumeration ---
	const actualShipped = [...index.shipped].sort();
	const pendingSlugs = index.pending.map((p) => p.slug);
	const expectedShipped = [...corpusSlugs].filter((s) => !pendingSlugs.includes(s)).sort();
	if (actualShipped.join(",") !== expectedShipped.join(",")) {
		problems.push(
			`[doc-c-index] shipped topic list must equal the enumerated corpus minus the pending list (computed ${corpusSlugs.size} topics): expected [${expectedShipped.join(", ")}], got [${actualShipped.join(", ")}]`,
		);
	}
	for (const slug of actualShipped) {
		if (pendingSlugs.includes(slug)) {
			problems.push(`[doc-c-index] topic "${slug}" is both shipped and pending`);
		}
	}
	for (const slug of pendingSlugs) {
		if (!corpusSlugs.has(slug)) {
			problems.push(`[doc-c-index] pending topic "${slug}" is not in the enumerated corpus`);
		}
	}
	const union = [...new Set([...actualShipped, ...pendingSlugs])].sort();
	const wholeCorpus = [...corpusSlugs].sort();
	if (union.join(",") !== wholeCorpus.join(",")) {
		problems.push(
			`[doc-c-index] shipped + pending must cover the enumerated corpus (computed ${wholeCorpus.length} topics): missing [${wholeCorpus.filter((s) => !union.includes(s)).join(", ")}]`,
		);
	}
	if (!TERMINAL_TOPIC_GATE_UNLOCKED) {
		for (const slug of terminal) {
			if (!pendingSlugs.includes(slug)) {
				problems.push(
					`[doc-c-index] terminal-visual topic "${slug}" must sit on the pending list while the gate is locked (${TERMINAL_TOPIC_GATE_REASON})`,
				);
			}
		}
	}
	for (const p of index.pending) {
		if (!(PENDING_REASONS as readonly string[]).includes(p.reason)) {
			problems.push(`[doc-c-index] pending topic "${p.slug}" carries unknown reason "${p.reason}"`);
		}
	}
	for (const slug of index.shipped) {
		if (!corpusSlugs.has(slug)) {
			problems.push(`[doc-c-index] shipped topic "${slug}" is not in the enumerated corpus`);
		}
	}

	// --- Per-topic doc + manifest + rows + fresh sidecars ---
	const rowsById = new Map(ctx.ledgerRows.map((r) => [r.id, r]));
	const seenFenceIds = new Map<string, string>();
	for (const slug of expectedShipped) {
		const docPath = topicDocPath(slug);
		const manPath = manifestPath(slug);
		if (!existsSync(resolve(root, docPath))) {
			problems.push(`[doc-c-topic] missing ported topic file ${docPath}`);
			continue;
		}
		if (!existsSync(resolve(root, manPath))) {
			problems.push(`[doc-c-topic] missing evidence manifest ${manPath}`);
			continue;
		}

		let manifest: TopicManifest;
		try {
			manifest = JSON.parse(readFileSync(resolve(root, manPath), "utf8")) as TopicManifest;
		} catch (error) {
			const detail = error instanceof Error ? error.message : String(error);
			problems.push(`[doc-c-manifest] ${manPath} is not valid JSON: ${detail}`);
			continue;
		}

		// Manifest shape
		if (manifest.schema !== MANIFEST_SCHEMA) {
			problems.push(`[doc-c-manifest] ${manPath} schema must be ${MANIFEST_SCHEMA}`);
		}
		if (manifest.topic !== slug || manifest.doc !== docPath) {
			problems.push(`[doc-c-manifest] ${manPath} must declare topic "${slug}" and doc "${docPath}"`);
		}
		const corpusTopic = corpus.find((t) => t.slug === slug);
		if (manifest.referenceSource !== corpusTopic?.referencePath) {
			problems.push(
				`[doc-c-manifest] ${manPath} referenceSource must be the enumerated corpus path ${corpusTopic?.referencePath}`,
			);
		}

		// Transcripts
		if (manifest.transcripts.length === 0) {
			problems.push(`[doc-c-manifest] ${manPath} binds no transcript artifact`);
		}
		for (const t of manifest.transcripts) {
			if (!(TRANSCRIPT_KINDS as readonly string[]).includes(t.kind)) {
				problems.push(`[doc-c-manifest] ${manPath} transcript kind "${t.kind}" is outside the closed set`);
				continue;
			}
			const canonical = transcriptSourceAllowed(t.kind, t.source);
			if (!canonical) {
				problems.push(
					`[doc-c-manifest] ${manPath} transcript ${t.source} is outside the canonical ${t.kind} artifact paths`,
				);
				continue;
			}
			const abs = resolve(root, t.source);
			if (!existsSync(abs)) {
				problems.push(
					`[doc-c-manifest] ${manPath} transcript artifact missing: ${t.source} (run the producing ${t.kind} harness first)`,
				);
				continue;
			}
			const maxAgeMs = t.kind === "cli-help" || t.kind === "release-flags" ? 10 * 60 * 1000 : 7 * 24 * 60 * 60 * 1000;
			const ageMs = Date.now() - statSync(abs).mtimeMs;
			if (ageMs > maxAgeMs) {
				problems.push(
					`[doc-c-manifest] ${manPath} transcript ${t.source} is not fresh (age ${Math.round(ageMs / 1000)}s); rerun the producing harness and the checker`,
				);
			}
		}

		// Pending reasons
		for (const p of manifest.pending) {
			if (!(PENDING_REASONS as readonly string[]).includes(p.reason)) {
				problems.push(`[doc-c-manifest] ${manPath} pending item "${p.item}" carries unknown reason "${p.reason}"`);
			}
		}

		// Claim rows exist, are DOC-C owned, and match manifest bindings
		for (const c of manifest.claims) {
			const row = rowsById.get(c.rowId);
			if (!row) {
				problems.push(`[doc-c-rows] ${manPath} references unknown ledger row ${c.rowId}`);
				continue;
			}
			if (row.owner !== "DOC-C") {
				problems.push(`[doc-c-rows] claim row ${c.rowId} must be owned by DOC-C`);
			}
			if (row.params.source !== c.source || row.params.claim !== c.claim) {
				problems.push(`[doc-c-rows] claim row ${c.rowId} params diverge from manifest binding`);
			}
		}

		// Topic review row
		const reviewRow = rowsById.get(`dc-topic-${slug}`);
		if (!reviewRow || reviewRow.class !== "review-only-prose" || reviewRow.params.source !== docPath) {
			problems.push(`[doc-c-rows] missing review-only-prose row dc-topic-${slug} for ${docPath}`);
		}

		// Fences
		const content = readFileSync(resolve(root, docPath), "utf8");
		const fences = extractTopicFences(content);
		let provenRows = 0;
		for (const fence of fences) {
			if (fence.markerId === "") {
				problems.push(`[doc-c-fence] ${docPath} has an unregistered fenced block (every fence needs ${FENCE_MARKER_PREFIX}<id>)`);
				continue;
			}
			if (seenFenceIds.has(fence.markerId)) {
				problems.push(
					`[doc-c-fence] fence id ${fence.markerId} appears in both ${seenFenceIds.get(fence.markerId)} and ${docPath}`,
				);
				continue;
			}
			seenFenceIds.set(fence.markerId, docPath);
			const rowId = `dc-fence-${fence.markerId}`;
			const row = rowsById.get(rowId);
			if (!row || row.class !== "fenced-compile" || row.params.topic !== docPath) {
				problems.push(`[doc-c-fence] fence ${fence.markerId} in ${docPath} has no matching fenced-compile row ${rowId}`);
				continue;
			}
			provenRows += 1;
			const badImports = scanFenceImports(fence);
			for (const bad of badImports) {
				problems.push(`[doc-c-imports] ${docPath} fence ${fence.markerId} imports "${bad}" outside the closed entrypoint set`);
			}
			if (fence.source !== null) {
				const abs = resolve(root, fence.source);
				if (!existsSync(abs)) {
					problems.push(`[doc-c-fence-source] ${docPath} fence ${fence.markerId} cites missing transcript ${fence.source}`);
				} else {
					const snapshot = readFileSync(abs, "utf8");
					const quote = fenceQuoteBody(fence);
					if (quote.length > 0 && !snapshot.includes(quote)) {
						problems.push(
							`[doc-c-fence-source] ${docPath} fence ${fence.markerId} does not quote ${fence.source} verbatim`,
						);
					}
				}
			}
		}
		if (provenRows === 0 && manifest.claims.length === 0) {
			problems.push(`[doc-c-topic] ${docPath} carries no registered fence or claim row`);
		}

		// Fresh sidecar requirement: at least one bound row executed this run
		const boundRows = [
			...manifest.claims.map((c) => c.rowId),
			reviewRow?.id,
			...fences.filter((f) => f.markerId !== "").map((f) => `dc-fence-${f.markerId}`),
		].filter((id): id is string => typeof id === "string");
		const fresh = boundRows.some((id) => ctx.freshRowIds.has(id));
		if (!fresh) {
			problems.push(`[doc-c-sidecar] ${docPath} has no ledger row with a fresh sidecar from the current run`);
		}
	}

	// --- Cross-ledger: every DOC-C fenced-compile row maps to a real fence ---
	for (const row of ctx.ledgerRows) {
		if (row.owner !== "DOC-C" || row.class !== "fenced-compile") continue;
		const topic = typeof row.params.topic === "string" ? row.params.topic : "";
		const marker = typeof row.params.fenceMarker === "string" ? row.params.fenceMarker : "";
		const id = marker.startsWith(FENCE_MARKER_PREFIX) ? marker.slice(FENCE_MARKER_PREFIX.length) : marker;
		const owner = seenFenceIds.get(id);
		if (owner === undefined) {
			problems.push(`[doc-c-fence] fenced-compile row ${row.id} has no fence ${FENCE_MARKER_PREFIX}${id} in any shipped topic`);
		} else if (owner !== topic) {
			problems.push(`[doc-c-fence] fenced-compile row ${row.id} claims topic ${topic} but the fence lives in ${owner}`);
		}
	}

	// --- Manifests must not exist for terminal topics while locked ---
	if (!TERMINAL_TOPIC_GATE_UNLOCKED) {
		for (const slug of terminal) {
			if (existsSync(resolve(root, manifestPath(slug)))) {
				problems.push(`[doc-c-terminal-gate] manifest exists for locked terminal topic: ${manifestPath(slug)}`);
			}
		}
	}

	// --- referencePin sanity: manifests must not fork the pin ---

	for (const slug of expectedShipped) {
		const manPath = manifestPath(slug);
		if (!existsSync(resolve(root, manPath))) continue;
		try {
			const manifest = JSON.parse(readFileSync(resolve(root, manPath), "utf8")) as TopicManifest & {
				referencePin?: string;
			};
			if (manifest.referencePin !== undefined && manifest.referencePin !== referencePin) {
				problems.push(
					`[doc-c-manifest] ${manPath} referencePin ${manifest.referencePin} forks the settled pin ${referencePin}`,
				);
			}
		} catch {
			// The per-topic loop already reported the invalid-JSON problem.
		}
	}

	return problems;
}

// ---------------------------------------------------------------------------
// Standalone entry (capture + check without the full ledger run)
// ---------------------------------------------------------------------------

export const DOCS_TOPICS_SENTINEL = "DOCS_TOPICS_OK";

async function main(): Promise<void> {
	const root = REPO_ROOT;
	captureCliHelpSnapshots(root);
	captureReleaseFlagSnapshot(root);
	mirrorHarnessArtifacts(root);
	const ledger = JSON.parse(readFileSync(resolve(root, "scripts/verification/docs-evidence.json"), "utf8")) as {
		referencePin: string;
		rows: { id: string; owner: string; surface: string; class: string; params: Record<string, unknown> }[];
	};
	const sidecarDir = resolve(root, "target/verification/docs-evidence");
	const fresh = new Set<string>();
	const staleCutoff = Date.now() - 7 * 24 * 60 * 60 * 1000;
	for (const row of ledger.rows) {
		if (row.owner !== "DOC-C") continue;
		const p = join(sidecarDir, `${row.id}.json`);
		if (!existsSync(p)) continue;
		const sidecar = JSON.parse(readFileSync(p, "utf8")) as { runId?: string };
		const runMs = Date.parse(sidecar.runId ?? "");
		if (Number.isFinite(runMs) && runMs >= staleCutoff) fresh.add(row.id);
	}
	const problems = checkDocsTopics({
		root,
		referencePin: ledger.referencePin,
		ledgerRows: ledger.rows,
		freshRowIds: fresh,
		sidecarDir,
	});
	if (problems.length === 0) {
		process.stdout.write(DOCS_TOPICS_SENTINEL + "\n");
		return;
	}
	console.error(`docs-topics: ${problems.length} problem(s):`);
	for (const p of problems) console.error(`  - ${p}`);
	process.exit(1);
}

if (import.meta.main) await main();
