#!/usr/bin/env bun
/**
 * Execution-map publisher (ARC-T2, issue #159).
 *
 * The live GitHub issue tree rooted at issue #12 is the authority. The source
 * fetch emits one normalized v2 envelope. This module validates that envelope
 * in memory and derives one Markdown bundle containing the registry and its
 * canonical JSON witness. The SHA-256 of the complete bundle bytes is its
 * generation ID.
 *
 * Publication installs the immutable content-addressed generation with an
 * atomic no-replace hard link. It then selects that generation by atomically
 * replacing `scripts/verification/fixtures/execution-map/current.md`. The
 * pointer swap is the commit point. Readers have no fallback path.
 *
 * Fetch and publication stay separate so the transform is reproducible
 * offline:
 *
 *   bun run scripts/verification/fetch-map-source.ts \
 *     | bun run scripts/verification/publish-map.ts
 */

import { randomUUID } from "node:crypto";
import {
	existsSync,
	linkSync,
	mkdirSync,
	readFileSync,
	renameSync,
	unlinkSync,
	writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { PINNED_AGENT_LOOP_CONFIG_SITES, REPO_ROOT } from "./parity.ts";
import {
	type ExpectedRow,
	EXECUTION_MAP_CURRENT_PATH,
	EXECUTION_MAP_GENERATIONS_DIRECTORY,
	EXECUTION_MAP_STAGING_DIRECTORY,
	computeExecutionMapGenerationId,
	computeSnapshotSourceHash,
	computeSnapshotStructuralHash,
	deriveExpectedRegistry,
	parseSnapshot,
	renderExecutionMapPointer,
} from "./map.ts";

const WITNESS_PATH = "scripts/verification/fixtures/execution-map-ticket-records.json";
const MAP_DOC_PATH = "docs/EXECUTION_MAP.md";
const REPOSITORY = "gosuda/pi-oxidized";

export interface PublisherRecord {
	readonly stableId: string;
	readonly kind: "execution" | "external";
	readonly modality: string;
	readonly issue: number;
	readonly url: string;
	readonly title: string;
	readonly question: string | null;
	readonly acceptance: string | null;
	readonly nativeParent: string;
	readonly blockers: readonly string[];
}

export interface WitnessEnvelope {
	readonly version: number;
	readonly repository: string;
	readonly canonicalIssue: number;
	readonly sourceRecordCount: number;
	readonly taskCount: number;
	readonly externalCount: number;
	readonly records: readonly PublisherRecord[];
}

function renderMapText(
	envelope: WitnessEnvelope,
	registry: readonly ExpectedRow[],
	sourceHash: string,
	structuralSha: string,
): string {
	const rows = registry.map((row) => {
		const blockers = row.blockedBy.length > 0 ? row.blockedBy.join(", ") : "—";
		return `| ${row.stableId} | ${row.modality} | #${row.issue} | ${row.title} | ${blockers} |`;
	});
	const externalCount = registry.filter((row) => row.recordKind === "external").length;
	const rowCount = rows.length;
	const siblingCount = envelope.taskCount - 6;
	const telemetrySites = PINNED_AGENT_LOOP_CONFIG_SITES.map(
		(site) => `- ${site.path}:${site.start}-${site.end}`,
	).join("\n");
	return `# Execution map

The canonical stable-ID DAG registry for the port program (MAP-1, issue #134). One row represents each ticket. The live GitHub issue tree rooted at #12 is authoritative. \`${EXECUTION_MAP_CURRENT_PATH}\` selects this immutable content-addressed generation, which contains both the derived view and its commit-pinned canonical v2 witness. \`bun run verify:map-ledger\` validates the complete generation digest and witness provenance, re-derives the ${rowCount}-row registry, checks every row's Issue, Title, and blocked_by field against the records exactly, and validates the mapped structural hash below.

- Snapshot structural sha256: \`${structuralSha}\`
- Witness source hash: \`${sourceHash}\` — the publisher's canonical SHA-256 (UTF-8 JSON, sorted keys, compact separators) over all ${envelope.sourceRecordCount} structural ticket records; mutable issue status is intentionally absent from the witness so issue closure never perturbs structural provenance.
- Row count: ${rowCount} — ${siblingCount} sibling graduate tickets (the legacy 109 plus the architecture siblings including the ARC-CLOSE closer), 6 map tickets MAP-1 through MAP-6, the ${externalCount} prerequisite external nodes, and MAP-ROOT for canonical issue #12.
- Published \`blocked_by\` cells never contain synthetic root edges: for a published row, \`A blocked_by B\` means prerequisite \`B\` feeds dependent \`A\`. During verification only, the checker adds \`MAP-ROOT -> F\` for each witness-derived canonical zero-blocker frontier row \`F\`; every registry row must be reachable from MAP-ROOT alone and must reach the terminal closure node MAP-6.
- REL-DOCS is registered exactly once and dominates documentation closure: REL-CLOSE and DOC-F are each blocked by it, and no REL-* node reaches DOC-F except through the REL-DOCS/REL-CLOSE gate.
- The prerequisite externals are the six named by MAP-1 (EXT-14, EXT-21, EXT-23, EXT-24, EXT-25, EXT-26) plus EXT-15, which XC-1 and externals EXT-24/EXT-25 cite.
- The architecture track (issues #153-#180) is registered by the same authority; its closer ARC-CLOSE is a required predecessor of the final cross-plan gate MAP-5 alongside the seven settled track closers.
- Modality vocabulary is pinned to the settled kinds of docs/PARITY_LEDGER.md (\`task\`, \`prototype\`, \`research\`, \`grilling\`, \`external\`); PAR-track rows exactly match the ledger's graduated parity-ticket DAG, all four graduation modalities stay populated, external rows are \`external\`, and MAP-ROOT is \`task\`. Modalities classify each ticket's graduation shape: \`research\` investigates and decides, \`grilling\` audits adversarially, \`prototype\` proves a harness or measurement, \`task\` executes.

## Registry

| Stable ID | Modality | Issue | Title | blocked_by |
| --- | --- | --- | --- | --- |
${rows.join("\n")}

## Pinned telemetry migration surface

Exactly ${PINNED_AGENT_LOOP_CONFIG_SITES.length} AgentLoopConfig struct-literal sites — the shared arbitration oracle, imported from \`PINNED_AGENT_LOOP_CONFIG_SITES\` in scripts/verification/parity.ts:

${telemetrySites}
`;
}

function buildWitnessText(envelope: WitnessEnvelope, sourceHash: string): string {
	return `${JSON.stringify(
		{
			version: envelope.version,
			repository: envelope.repository,
			canonicalIssue: envelope.canonicalIssue,
			sourceHash,
			sourceRecordCount: envelope.sourceRecordCount,
			taskCount: envelope.taskCount,
			externalCount: envelope.externalCount,
			records: envelope.records,
		},
		null,
		2,
	)}\n`;
}

/**
 * Validate the stdin envelope and render the canonical witness and map text in
 * memory. Rejection here guarantees no file-system write.
 *
 * Validation is fatal on any structural problem: a null snapshot, any
 * non-empty `parseSnapshot().problems` (malformed records, invalid
 * modalities), or declared counts that do not match the actual validated
 * records. Malformed envelope metadata cannot reach publication.
 */
export function validateAndRender(envelope: WitnessEnvelope): {
	witnessText: string;
	mapText: string;
	sourceHash: string;
	structuralSha: string;
} {
	const records = envelope.records;
	if (!Array.isArray(records) || records.length === 0) {
		throw new Error("source records array is empty");
	}
	if (envelope.version !== 2 || envelope.repository !== REPOSITORY || envelope.canonicalIssue !== 12) {
		throw new Error("source envelope identity pins drifted");
	}
	const sourceHash = computeSnapshotSourceHash(records);
	const witnessText = buildWitnessText(envelope, sourceHash);
	const parsed = parseSnapshot(witnessText);
	if (parsed.snapshot === null) {
		throw new Error(`regenerated witness does not parse: ${parsed.problems.join("; ")}`);
	}
	if (parsed.problems.length > 0) {
		throw new Error(`regenerated witness has validation problems: ${parsed.problems.join("; ")}`);
	}
	const snapshot = parsed.snapshot;
	const actualRecordCount = snapshot.records.length;
	const actualTaskCount = snapshot.records.filter((record) => record.kind === "execution").length;
	const actualExternalCount = snapshot.records.filter((record) => record.kind === "external").length;
	if (snapshot.sourceRecordCount !== actualRecordCount) {
		throw new Error(
			`declared sourceRecordCount ${snapshot.sourceRecordCount} does not match validated records ${actualRecordCount}`,
		);
	}
	if (snapshot.taskCount !== actualTaskCount) {
		throw new Error(
			`declared taskCount ${snapshot.taskCount} does not match validated execution records ${actualTaskCount}`,
		);
	}
	if (snapshot.externalCount !== actualExternalCount) {
		throw new Error(
			`declared externalCount ${snapshot.externalCount} does not match validated external records ${actualExternalCount}`,
		);
	}
	const derived = deriveExpectedRegistry(snapshot);
	if (derived.problems.length > 0) {
		throw new Error(`cannot derive execution-map registry: ${derived.problems.join("; ")}`);
	}
	const structuralSha = computeSnapshotStructuralHash(snapshot);
	const mapText = renderMapText(envelope, derived.rows, sourceHash, structuralSha);
	return { witnessText, mapText, sourceHash, structuralSha };
}

export interface ExecutionMapGeneration {
	readonly bundleText: string;
	readonly generationId: string;
	readonly sourceHash: string;
	readonly structuralSha: string;
	readonly recordCount: number;
}

/** Render the complete immutable publication unit before touching the filesystem. */
export function renderExecutionMapGeneration(envelope: WitnessEnvelope): ExecutionMapGeneration {
	const rendered = validateAndRender(envelope);
	const bundleText = `${rendered.mapText}\n## Canonical witness\n\n\`\`\`json\n${rendered.witnessText}\`\`\`\n`;
	return {
		bundleText,
		generationId: computeExecutionMapGenerationId(bundleText),
		sourceHash: rendered.sourceHash,
		structuralSha: rendered.structuralSha,
		recordCount: envelope.records.length,
	};
}

/** The exact synchronous operations used by immutable install and pointer selection. */
export interface ExecutionMapFilesystem {
	mkdir(path: string): void;
	read(path: string): string;
	write(path: string, data: string): void;
	link(existingPath: string, newPath: string): void;
	rename(from: string, to: string): void;
	unlink(path: string): void;
	exists(path: string): boolean;
}

const executionMapFilesystem: ExecutionMapFilesystem = {
	mkdir: (path) => mkdirSync(path, { recursive: true }),
	read: (path) => readFileSync(path, "utf8"),
	write: (path, data) => writeFileSync(path, data, "utf8"),
	link: linkSync,
	rename: renameSync,
	unlink: unlinkSync,
	exists: existsSync,
};

function cleanupKnownStages(fs: ExecutionMapFilesystem, stages: readonly string[]): unknown[] {
	const failures: unknown[] = [];
	for (const stage of stages) {
		try {
			if (fs.exists(stage)) fs.unlink(stage);
		} catch (error) {
			failures.push(error);
		}
	}
	return failures;
}

function throwWithCleanup(primary: unknown, cleanupFailures: readonly unknown[]): never {
	if (cleanupFailures.length > 0) {
		throw new AggregateError([primary, ...cleanupFailures], "execution-map publication and staging cleanup failed");
	}
	throw primary;
}

/** Install one immutable generation, then atomically select it with current.md. */
export function publishExecutionMap(
	envelope: WitnessEnvelope,
	fs: ExecutionMapFilesystem,
	rootDir: string,
): ExecutionMapGeneration {
	const generation = renderExecutionMapGeneration(envelope);
	const generationsDirectory = join(rootDir, EXECUTION_MAP_GENERATIONS_DIRECTORY);
	const stagingDirectory = join(rootDir, EXECUTION_MAP_STAGING_DIRECTORY);
	const generationPath = join(generationsDirectory, `${generation.generationId}.md`);
	const pointerPath = join(rootDir, EXECUTION_MAP_CURRENT_PATH);
	const pointerText = renderExecutionMapPointer(generation.generationId);
	const suffix = randomUUID();
	const knownStages: string[] = [];

	try {
		fs.mkdir(generationsDirectory);
		fs.mkdir(stagingDirectory);

		const generationStage = join(stagingDirectory, `generation-${suffix}.md`);
		knownStages.push(generationStage);
		fs.write(generationStage, generation.bundleText);
		try {
			fs.link(generationStage, generationPath);
		} catch (linkFailure) {
			let installed = false;
			try {
				const target = fs.read(generationPath);
				installed = target === generation.bundleText && computeExecutionMapGenerationId(target) === generation.generationId;
			} catch {
				installed = false;
			}
			if (!installed) throw linkFailure;
		}
		fs.unlink(generationStage);
		knownStages.pop();

		const pointerStage = join(stagingDirectory, `pointer-${suffix}.md`);
		knownStages.push(pointerStage);
		fs.write(pointerStage, pointerText);
		try {
			fs.rename(pointerStage, pointerPath);
		} catch (renameFailure) {
			let selected = false;
			try {
				selected = fs.read(pointerPath) === pointerText;
			} catch {
				selected = false;
			}
			if (!selected) throw renameFailure;
		}
		return generation;
	} catch (primaryFailure) {
		throwWithCleanup(primaryFailure, cleanupKnownStages(fs, knownStages));
	}
}

/**
 * Filesystem operations the publication transaction depends on. This seam
 * exists solely to protect the two-output publication invariant and to allow
 * failure-injection tests; it is not a general filesystem abstraction.
 */
export interface PublicationFilesystem {
	writeFileSync(path: string, data: string): void;
	renameSync(from: string, to: string): void;
	unlinkSync(path: string): void;
	existsSync(path: string): boolean;
}

export interface PublicationDestination {
	readonly path: string;
	readonly content: string;
}

const realFilesystem: PublicationFilesystem = {
	writeFileSync,
	renameSync,
	unlinkSync,
	existsSync,
};

let transactionCounter = 0;
function transactionSuffix(): string {
	transactionCounter += 1;
	return `${process.pid.toString(36)}-${transactionCounter.toString(36)}`;
}

/**
 * Two-output publication transaction. Materialize both complete outputs into
 * unique same-directory staging files, preserve both prior destinations as
 * unique same-directory backups, install both staged files with synchronous
 * same-directory renames, then remove backups.
 *
 * No destination is ever written directly: only same-directory renames move
 * fully-materialized content into place. If any staging, backup, or install
 * operation throws, restore every prior destination from its backup (or
 * remove it if it had no prior file), remove any staged partial, and rethrow
 * the primary failure. If rollback also fails, surface both the primary and
 * rollback failures rather than hiding either.
 */
export function publishPair(fs: PublicationFilesystem, destinations: readonly PublicationDestination[]): void {
	if (destinations.length === 0) throw new Error("publishPair requires at least one destination");
	const suffix = transactionSuffix();
	const staged: string[] = [];
	const backups: Array<{ backup: string; original: string }> = [];
	const noPrior: string[] = [];
	try {
		for (const dest of destinations) {
			const stagingPath = `${dest.path}.stage-${suffix}`;
			fs.writeFileSync(stagingPath, dest.content);
			staged.push(stagingPath);
		}
		for (const dest of destinations) {
			if (fs.existsSync(dest.path)) {
				const backupPath = `${dest.path}.backup-${suffix}`;
				fs.renameSync(dest.path, backupPath);
				backups.push({ backup: backupPath, original: dest.path });
			} else {
				noPrior.push(dest.path);
			}
		}
		for (let i = 0; i < destinations.length; i += 1) {
			const stagingPath = staged[i];
			const dest = destinations[i];
			if (stagingPath === undefined || dest === undefined) {
				throw new Error("publishPair: staging invariant violated — staged/destination index mismatch");
			}
			fs.renameSync(stagingPath, dest.path);
			staged[i] = "";
		}
		for (const backup of backups) {
			fs.unlinkSync(backup.backup);
		}
	} catch (primaryFailure) {
		const rollbackErrors: unknown[] = [];
		for (let bi = backups.length - 1; bi >= 0; bi -= 1) {
			const backup = backups[bi];
			if (backup === undefined) continue;
			try {
				if (fs.existsSync(backup.original)) fs.unlinkSync(backup.original);
				fs.renameSync(backup.backup, backup.original);
			} catch (rollbackError) {
				rollbackErrors.push(rollbackError);
			}
		}
		for (const path of noPrior) {
			try {
				if (fs.existsSync(path)) fs.unlinkSync(path);
			} catch (rollbackError) {
				rollbackErrors.push(rollbackError);
			}
		}
		for (const stagingPath of staged) {
			if (stagingPath === "") continue;
			try {
				fs.unlinkSync(stagingPath);
			} catch (rollbackError) {
				rollbackErrors.push(rollbackError);
			}
		}
		if (rollbackErrors.length > 0) {
			throw new AggregateError(
				[primaryFailure, ...rollbackErrors],
				"publishPair: publication failed and rollback could not fully restore prior destinations",
			);
		}
		throw primaryFailure;
	}
}

/**
 * Legacy two-output pair publication. Superseded by publishExecutionMap;
 * retained only until the deletion-only cleanup commit removes it together
 * with publishPair, its destination type, and the old adapter.
 */
export function publishFromEnvelope(
	envelope: WitnessEnvelope,
	fs: PublicationFilesystem,
	rootDir: string,
): { sourceHash: string; structuralSha: string; recordCount: number } {
	const { witnessText, mapText, sourceHash, structuralSha } = validateAndRender(envelope);
	publishPair(fs, [
		{ path: join(rootDir, WITNESS_PATH), content: witnessText },
		{ path: join(rootDir, MAP_DOC_PATH), content: mapText },
	]);
	return { sourceHash, structuralSha, recordCount: envelope.records.length };
}

async function main(): Promise<void> {
	const stdin = await new Promise<string>((resolveText, reject) => {
		let data = "";
		process.stdin.setEncoding("utf8");
		process.stdin.on("data", (chunk) => {
			data += chunk;
		});
		process.stdin.on("end", () => resolveText(data));
		process.stdin.on("error", reject);
	});
	if (stdin.trim() === "") {
		console.error("publish-map: no source records on stdin; pipe the fetch-map-source output in");
		process.exit(1);
	}
	let envelope: WitnessEnvelope;
	try {
		envelope = JSON.parse(stdin) as WitnessEnvelope;
	} catch (error) {
		console.error(`publish-map: stdin is not valid JSON: ${String(error)}`);
		process.exit(1);
	}
	let result: ExecutionMapGeneration;
	try {
		result = publishExecutionMap(envelope, executionMapFilesystem, REPO_ROOT);
	} catch (error) {
		console.error(`publish-map: ${String(error)}`);
		process.exit(1);
	}
	console.error(
		`publish-map: selected generation ${result.generationId} ` +
			`(${result.recordCount} records, source ${result.sourceHash}, structural ${result.structuralSha})`,
	);
}

if (import.meta.main) {
	await main();
}
