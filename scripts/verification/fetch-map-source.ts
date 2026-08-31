#!/usr/bin/env bun
/**
 * Live-tree source fetch for the execution-map publisher (ARC-T2, issue #159).
 *
 * The canonical authority is the live GitHub issue tree rooted at issue #12.
 * This fetch walks that tree over the GitHub API exactly as the publisher
 * owner's journal does — sub-issue edges, canonical `Stable ID:` headers,
 * canonical `Blocked by` bullet links, `## Question`/`## Acceptance`
 * graduation sections, and the single `wayfinder:*` type label — and emits
 * the normalized structural records as the v2 witness envelope on stdout:
 *
 *   { version, repository, canonicalIssue, sourceRecordCount, taskCount,
 *     externalCount, records: [...] }
 *
 * Each record carries an explicit `modality` field derived from the issue's
 * single `wayfinder:*` label (`task`, `prototype`, `research`, or `grilling`)
 * for execution tickets, or `external` for external records.
 *
 * Pipe it into the publisher to install one immutable execution-map generation
 * (selected by the current-generation pointer) offline-deterministically:
 *
 *   bun run scripts/verification/fetch-map-source.ts \
 *     | bun run scripts/verification/publish-map.ts
 *
 * Mutable issue state (`issueState`/`resolved`) is fetched for validation but
 * is deliberately excluded from the emitted structural records, so issue
 * closure never perturbs the witness provenance hash.
 */

import { REPO_ROOT } from "./parity.ts";

const REPOSITORY = "gosuda/pi-oxidized";
const ROOT_ISSUE = 12;
const EXTERNAL_RANGE: ReadonlySet<number> = new Set(
	Array.from({ length: 16 }, (_unused, index) => 13 + index),
);
const GENERIC_ACCEPTANCE =
	"- The ticket question is resolved with repository evidence and its parent plan's acceptance gates pass.";

const STABLE_ID_PATTERN = /^Stable ID:\s*`([^`]+)`\s*$/m;
const ISSUE_LINK_PATTERN = /https:\/\/github\.com\/(?:metaphorics|gosuda)\/pi-oxidized\/issues\/(\d+)/g;
const NONE_BULLET_PATTERN = /^-\s*None\.?$/i;

/** The four execution-ticket modalities, each mapped from one wayfinder label. */
const WAYFINDER_LABEL_TO_MODALITY: ReadonlyMap<string, string> = new Map([
	["wayfinder:task", "task"],
	["wayfinder:prototype", "prototype"],
	["wayfinder:research", "research"],
	["wayfinder:grilling", "grilling"],
]);

interface IssuePayload {
	readonly number: number;
	readonly title: string;
	readonly state: string;
	readonly body: string | null;
	readonly html_url: string;
	readonly labels: readonly { readonly name: string }[];
}

type Modality = "task" | "prototype" | "research" | "grilling" | "external";

interface NormalizedRecord {
	readonly stableId: string;
	readonly kind: "execution" | "external";
	readonly modality: Modality;
	readonly issue: number;
	readonly url: string;
	readonly title: string;
	readonly question: string | null;
	readonly acceptance: string | null;
	readonly nativeParent: string;
	readonly blockers: readonly string[];
	readonly issueState: "open" | "closed";
	readonly resolved: boolean;
}

function fail(message: string): never {
	throw new Error(message);
}

function run(argv: readonly string[]): string {
	const result = Bun.spawnSync([...argv], { cwd: REPO_ROOT, stdout: "pipe", stderr: "pipe" });
	if (result.exitCode !== 0) {
		fail(`command failed (${result.exitCode}): ${JSON.stringify(argv)}\n${result.stderr.toString().trim()}`);
	}
	return result.stdout.toString();
}

function ghJson(argv: readonly string[]): unknown {
	try {
		return JSON.parse(run(["gh", ...argv]));
	} catch (error) {
		if (error instanceof SyntaxError) fail(`gh returned invalid JSON: ${String(error)}`);
		throw error;
	}
}

/**
 * Line-based section extractor: finds an exact `## <heading>` line and
 * collects every line through the line before the next `## ` heading or
 * EOF. Unlike a regex, this preserves every bullet in multi-bullet
 * `Blocked by` sections.
 */
export function section(body: string, heading: string, required: boolean): string | null {
	const lines = body.split("\n");
	const prefix = `## ${heading}`;
	let start = -1;
	for (let index = 0; index < lines.length; index += 1) {
		if (lines[index]?.trimEnd() === prefix) {
			start = index + 1;
			break;
		}
	}
	if (start < 0) {
		if (required) fail(`missing ${heading} section`);
		return null;
	}
	const collected: string[] = [];
	for (let index = start; index < lines.length; index += 1) {
		const line = lines[index];
		if (line !== undefined && line.startsWith("## ")) break;
		collected.push(line ?? "");
	}
	const value = collected.join("\n").trim();
	if (required && value === "") fail(`missing ${heading} section`);
	return value;
}

function blockerNumbers(body: string, issue: number): number[] {
	const text = section(body, "Blocked by", true) ?? "";
	const bullets = text
		.split("\n")
		.map((line) => line.trim())
		.filter((line) => line.startsWith("-"));
	if (bullets.length === 0) fail(`issue #${issue}: Blocked by has no bullets`);
	if (bullets.every((bullet) => NONE_BULLET_PATTERN.test(bullet))) return [];
	const numbers: number[] = [];
	for (const bullet of bullets) {
		const links = [...bullet.matchAll(ISSUE_LINK_PATTERN)].map((match) => Number(match[1]));
		if (links.length !== 1) fail(`issue #${issue}: blocker must be one canonical issue link: ${bullet}`);
		const link = links[0];
		if (link === undefined) fail("unreachable");
		numbers.push(link);
	}
	if (new Set(numbers).size !== numbers.length) fail(`issue #${issue}: duplicate blockers`);
	return numbers;
}

function fetchChildren(issue: number): IssuePayload[] {
	const pages = ghJson(["api", "--paginate", "--slurp", `repos/${REPOSITORY}/issues/${issue}/sub_issues`]);
	if (!Array.isArray(pages)) fail(`issue #${issue}: sub-issues response is not pages`);
	const children: IssuePayload[] = [];
	for (const page of pages) {
		if (!Array.isArray(page)) fail(`issue #${issue}: sub-issues page is not an array`);
		for (const child of page) {
			if (typeof child !== "object" || child === null) fail(`issue #${issue}: invalid sub-issue record`);
			children.push(child as IssuePayload);
		}
	}
	return children;
}

function fetchLiveTree(): { records: Map<number, IssuePayload>; parents: Map<number, number> } {
	const records = new Map<number, IssuePayload>();
	const parents = new Map<number, number>();
	const seen = new Set<number>();
	let frontier: number[] = [ROOT_ISSUE];
	while (frontier.length > 0) {
		const batch = [...frontier].filter((issue) => !seen.has(issue)).sort((left, right) => left - right);
		if (batch.length === 0) break;
		for (const issue of batch) seen.add(issue);
		const next: number[] = [];
		for (const parent of batch) {
			for (const child of fetchChildren(parent)) {
				const number = child.number;
				if (typeof number !== "number") fail(`issue #${parent}: child has no number`);
				const prior = parents.get(number);
				if (prior !== undefined && prior !== parent) {
					fail(`issue #${number}: multiple native parents #${prior} and #${parent}`);
				}
				records.set(number, child);
				parents.set(number, parent);
				next.push(number);
			}
		}
		frontier = next;
	}
	return { records, parents };
}

function modalityForIssue(issue: IssuePayload, number: number, external: boolean): Modality {
	if (external) return "external";
	const labels = issue.labels ?? [];
	const matched: string[] = [];
	for (const label of labels) {
		const name = label.name;
		if (typeof name !== "string") continue;
		if (WAYFINDER_LABEL_TO_MODALITY.has(name)) matched.push(name);
	}
	if (matched.length === 0) {
		fail(`issue #${number}: missing wayfinder type label (expected exactly one of ${[...WAYFINDER_LABEL_TO_MODALITY.keys()].join(", ")})`);
	}
	if (matched.length > 1) {
		fail(`issue #${number}: conflicting wayfinder type labels ${matched.join(", ")} (expected exactly one)`);
	}
	const modality = WAYFINDER_LABEL_TO_MODALITY.get(matched[0] ?? fail("unreachable"));
	if (modality === undefined) fail(`issue #${number}: unknown wayfinder label ${matched[0]}`);
	return modality as Modality;
}

function normalizeLive(
	records: ReadonlyMap<number, IssuePayload>,
	parents: ReadonlyMap<number, number>,
): NormalizedRecord[] {
	const identities = new Map<number, string>();
	for (const [number, issue] of records) {
		const body = issue.body;
		if (typeof body !== "string") fail(`issue #${number}: body is not text`);
		const stable = body.match(STABLE_ID_PATTERN);
		identities.set(number, stable === null ? `EXT-${number}` : (stable[1] ?? fail("unreachable")));
	}
	const normalized: NormalizedRecord[] = [];
	const stableIds = new Set<string>();
	for (const number of [...records.keys()].sort((left, right) => left - right)) {
		const issue = records.get(number) ?? fail("unreachable");
		const body = issue.body ?? "";
		const stableId = identities.get(number) ?? fail("unreachable");
		const external = EXTERNAL_RANGE.has(number);
		if (stableIds.has(stableId)) fail(`duplicate Stable ID ${stableId}`);
		stableIds.add(stableId);
		// Decision-form tickets (research/grilling) record "Resolution to
		// record" instead of "## Acceptance"; the graduation witness then
		// carries the parent plan's generic acceptance contract.
		if (!external && STABLE_ID_PATTERN.test(body) === false) fail(`issue #${number}: missing Stable ID`);
		const blockers = blockerNumbers(body, number);
		const unknown = blockers.filter((blocker) => !records.has(blocker));
		if (unknown.length > 0) fail(`issue #${number}: unknown blockers ${unknown.sort()}`);
		const state = issue.state;
		if (state !== "open" && state !== "closed") fail(`issue #${number}: invalid state ${JSON.stringify(state)}`);
		if (external && state !== "closed") fail(`${stableId}: charting input is not resolved`);
		const modality = modalityForIssue(issue, number, external);
		let acceptance = section(body, "Acceptance", false);
		if (!external && (acceptance === null || acceptance === "")) acceptance = GENERIC_ACCEPTANCE;
		const parentNumber = parents.get(number);
		if (parentNumber === undefined) fail(`issue #${number}: missing native parent`);
		const parentId = parentNumber === ROOT_ISSUE ? "ROOT-12" : identities.get(parentNumber);
		if (parentId === undefined) fail(`issue #${number}: unknown native parent #${parentNumber}`);
		normalized.push({
			stableId,
			kind: external ? "external" : "execution",
			modality,
			issue: number,
			url: issue.html_url,
			title: issue.title,
			question: section(body, "Question", true),
			acceptance,
			nativeParent: parentId,
			blockers: blockers.map((blocker) => identities.get(blocker) ?? fail("unreachable")),
			issueState: state,
			resolved: state === "closed",
		});
	}
	return normalized;
}

if (import.meta.main) {
	const { records, parents } = fetchLiveTree();
	const normalized = normalizeLive(records, parents);
	const envelope = {
		version: 2,
		repository: REPOSITORY,
		canonicalIssue: ROOT_ISSUE,
		sourceRecordCount: normalized.length,
		taskCount: normalized.filter((record) => record.kind === "execution").length,
		externalCount: normalized.filter((record) => record.kind === "external").length,
		records: normalized.map(({ issueState: _issueState, resolved: _resolved, ...structural }) => structural),
	};
	process.stdout.write(`${JSON.stringify(envelope, null, 2)}\n`);
}
