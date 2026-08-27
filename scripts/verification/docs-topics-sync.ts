#!/usr/bin/env bun
/**
 * DOC-C ledger/inventory sync (issue #137).
 *
 * Regenerates the DOC-C portion of the doc-evidence ledger and its inventory
 * surfaces from the shipped topics themselves:
 *
 *   - one review-only-prose row per shipped topic plus docs/index.md,
 *   - one fenced-compile row per `doc-c:fence=<id>` marker found in a topic,
 *   - one transcript-claim row per claim bound in docs/evidence/<topic>.json.
 *
 * DOC-A-owned rows and inventory categories are preserved verbatim. Output is
 * deterministic: running sync twice with unchanged inputs is a no-op.
 */

import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import {
	FENCE_MARKER_PREFIX,
	MANIFEST_SCHEMA,
	enumerateCorpusTopics,
	extractTopicFences,
	manifestPath,
	parseIndex,
	topicDocPath,
} from "./docs-topics.ts";

const REPO_ROOT = resolve(import.meta.dirname, "../..");
const LEDGER_PATH = "scripts/verification/docs-evidence.json";
const INVENTORY_PATH = "scripts/verification/fixtures/docs-inventory.json";
const INVENTORY_CATEGORY_ID = "ported-user-docs";

interface LedgerRow {
	readonly id: string;
	readonly surface: string;
	readonly owner: string;
	readonly class: string;
	readonly params: Readonly<Record<string, unknown>>;
}

interface Ledger {
	schema: string;
	referencePin: string;
	rows: LedgerRow[];
}

interface InventoryCategory {
	readonly id: string;
	readonly name: string;
	readonly surfaces: readonly string[];
}

interface InventoryArtifact {
	readonly schema: string;
	readonly source: string;
	readonly categories: InventoryCategory[];
}

interface TopicManifest {
	readonly schema: string;
	readonly topic: string;
	readonly claims: readonly { rowId: string; source: string; claim: string }[];
}

function readJson(rel: string): unknown {
	return JSON.parse(readFileSync(resolve(REPO_ROOT, rel), "utf8"));
}

function buildDocCRows(): { rows: LedgerRow[]; surfaces: string[] } {
	const corpusSlugs = new Set(enumerateCorpusTopics(REPO_ROOT).map((t) => t.slug));
	const index = parseIndex(readFileSync(resolve(REPO_ROOT, "docs/index.md"), "utf8"));
	const rows: LedgerRow[] = [
		{
			id: "dc-index",
			surface: "docs/index.md",
			owner: "DOC-C",
			class: "review-only-prose",
			params: { source: "docs/index.md" },
		},
	];

	for (const slug of index.shipped) {
		if (!corpusSlugs.has(slug)) {
			throw new Error(`sync: shipped topic "${slug}" is not in the enumerated corpus`);
		}
		const docPath = topicDocPath(slug);
		if (!existsSync(resolve(REPO_ROOT, docPath))) {
			throw new Error(`sync: shipped topic file missing: ${docPath}`);
		}
		rows.push({
			id: `dc-topic-${slug}`,
			surface: docPath,
			owner: "DOC-C",
			class: "review-only-prose",
			params: { source: docPath },
		});

		const manifest = readJson(manifestPath(slug)) as TopicManifest;
		if (manifest.schema !== MANIFEST_SCHEMA) {
			throw new Error(`sync: ${manifestPath(slug)} schema must be ${MANIFEST_SCHEMA}`);
		}
		for (const claim of manifest.claims) {
			rows.push({
				id: claim.rowId,
				surface: `${claim.source}#claim`,
				owner: "DOC-C",
				class: "transcript-claim",
				params: { source: claim.source, claim: claim.claim },
			});
		}

		const content = readFileSync(resolve(REPO_ROOT, docPath), "utf8");
		for (const fence of extractTopicFences(content)) {
			if (fence.markerId === "") {
				throw new Error(`sync: ${docPath} has an unregistered fenced block`);
			}
			rows.push({
				id: `dc-fence-${fence.markerId}`,
				surface: `${docPath}#fence/${fence.markerId}`,
				owner: "DOC-C",
				class: "fenced-compile",
				params: { topic: docPath, fenceMarker: `${FENCE_MARKER_PREFIX}${fence.markerId}` },
			});
		}
	}

	const surfaces = rows.map((row) => row.surface);
	return { rows, surfaces };
}

function main(): void {
	const ledger = readJson(LEDGER_PATH) as Ledger;
	const inventory = readJson(INVENTORY_PATH) as InventoryArtifact;
	const { rows, surfaces } = buildDocCRows();

	const keptRows = ledger.rows.filter((row) => row.owner !== "DOC-C");
	const nextRows = [...keptRows, ...rows];
	writeFileSync(
		resolve(REPO_ROOT, LEDGER_PATH),
		`${JSON.stringify({ ...ledger, rows: nextRows }, null, "\t")}\n`,
		"utf8",
	);

	const keptCategories = inventory.categories.filter((c) => c.id !== INVENTORY_CATEGORY_ID);
	const nextCategories = [
		...keptCategories,
		{
			id: INVENTORY_CATEGORY_ID,
			name: "Ported user-doc topics (DOC-C) — one surface per DOC-C ledger row",
			surfaces,
		},
	];
	writeFileSync(
		resolve(REPO_ROOT, INVENTORY_PATH),
		`${JSON.stringify({ ...inventory, categories: nextCategories }, null, "\t")}\n`,
		"utf8",
	);

	process.stdout.write(`docs-topics-sync: ${rows.length} DOC-C rows (${nextRows.length} total)\n`);
}

main();
