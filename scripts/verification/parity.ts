#!/usr/bin/env bun
/**
 * Parity ledger boundary witness suite (PAR-LEDGER, issue #29).
 *
 * Five exported witness operations freeze the boundaries that
 * docs/PARITY_LEDGER.md pins, so any drift fails one command:
 *
 * 1. verifyWorkspaceTopology    - exactly five workspace members and the fixed
 *                                 internal edge set.
 * 2. verifyCrateBoundaries      - pi-agent never references pi_ext/pi_tui and
 *                                 pi-ext never references the pi crate.
 * 3. verifyCapabilityLedger     - 57 structurally unique capability IDs with
 *                                 the A8/A9/E1/R1-R4 special contracts.
 * 4. verifyGraduatedTicketDag   - stable IDs, every blocked_by resolvable, no
 *                                 cycles, no missing nodes.
 * 5. verifyAgentLoopConfigSites - exactly five AgentLoopConfig literal sites
 *                                 at the pinned files and line ranges.
 *
 * Each operation is a pure check over text or collected inputs so the test
 * suite can drive malformed, duplicate, and drift fixtures through them.
 * runParityWitnesses composes all five against one repository root and is the
 * single acceptance path behind `bun run verify:parity`.
 */

import { readdirSync, readFileSync } from "node:fs";
import type { Dirent } from "node:fs";
import { join, resolve } from "node:path";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");

export const FIXED_WORKSPACE_MEMBERS: readonly string[] = ["pi", "pi-agent", "pi-ai", "pi-ext", "pi-tui"];

export const FIXED_INTERNAL_EDGES: readonly string[] = [
	"pi-agent -> pi-ai",
	"pi -> pi-agent",
	"pi -> pi-ai",
	"pi -> pi-ext",
	"pi -> pi-tui",
	"pi-ext -> pi-agent",
	"pi-ext -> pi-ai",
	"pi-ext -> pi-tui",
];

export interface AgentLoopConfigPin {
	readonly path: string;
	readonly start: number;
	readonly end: number;
}

/** The shared arbitration oracle: every full AgentLoopConfig construction site. */
export const PINNED_AGENT_LOOP_CONFIG_SITES: readonly AgentLoopConfigPin[] = [
	{ path: "crates/pi-agent/src/agent.rs", start: 62, end: 88 },
	{ path: "crates/pi-agent/src/config.rs", start: 360, end: 389 },
	{ path: "crates/pi-agent/src/run.rs", start: 835, end: 861 },
	{ path: "crates/pi-agent/src/schedule.rs", start: 902, end: 928 },
	{ path: "crates/pi/src/core/agent_session/mod.rs", start: 463, end: 489 },
];

// ============================================================================
// Witness 1: workspace topology
// ============================================================================

export interface WorkspaceTopology {
	readonly members: readonly string[];
	readonly edges: readonly string[];
	readonly problems: readonly string[];
}

/** Crate names come from the basename of each `members = [...]` entry. */
export function parseWorkspaceMembers(rootManifest: string): string[] {
	const membersMatch = rootManifest.match(/members\s*=\s*\[([^\]]*)\]/);
	if (membersMatch === null) return [];
	const body = membersMatch[1] ?? "";
	return body
		.split(",")
		.map((entry) => entry.trim().replace(/^"|"$/g, ""))
		.filter((entry) => entry !== "")
		.map((entry) => entry.split("/").pop() ?? entry);
}

/**
 * Internal dependency names from every dependency table
 * (`[dependencies]`, `[dev-dependencies]`, `[target...dependencies]`, ...).
 * Membership filtering keeps multi-line entry bodies from faking keys.
 */

type TomlValue = string | number | boolean | Date | TomlTable | TomlValue[];

interface TomlTable {
	readonly [key: string]: TomlValue;
}

const parseToml = Bun.TOML.parse as (source: string) => TomlTable;
const DEPENDENCY_TABLES = ["dependencies", "dev-dependencies", "build-dependencies"] as const;

function asTomlTable(value: TomlValue | undefined): TomlTable | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value) || value instanceof Date) return undefined;
	return value;
}

function collectDependencyTable(
	value: TomlValue | undefined,
	members: ReadonlySet<string>,
	names: Set<string>,
	workspaceDependencies: TomlTable | undefined,
): void {
	const table = asTomlTable(value);
	if (table === undefined) return;
	for (const [key, specification] of Object.entries(table)) {
		const fields = asTomlTable(specification);
		const inherited =
			fields?.workspace === true ? asTomlTable(workspaceDependencies?.[key]) : undefined;
		const packageName =
			typeof inherited?.package === "string"
				? inherited.package
				: typeof fields?.package === "string"
					? fields.package
					: key;
		if (members.has(packageName)) names.add(packageName);
	}
}

function collectDependencyGroups(
	table: TomlTable,
	members: ReadonlySet<string>,
	names: Set<string>,
	workspaceDependencies: TomlTable | undefined,
): void {
	for (const name of DEPENDENCY_TABLES) {
		collectDependencyTable(table[name], members, names, workspaceDependencies);
	}
}

export function parseInternalDependencyNames(
	crateManifest: string,
	members: readonly string[],
	rootManifest?: string,
): string[] {
	let manifest: TomlTable;
	let workspaceDependencies: TomlTable | undefined;
	try {
		manifest = parseToml(crateManifest);
		if (rootManifest !== undefined) {
			const workspace = asTomlTable(parseToml(rootManifest).workspace);
			workspaceDependencies = asTomlTable(workspace?.dependencies);
		}
	} catch {
		return [];
	}
	const names = new Set<string>();
	const memberSet = new Set(members);
	collectDependencyGroups(manifest, memberSet, names, workspaceDependencies);
	const targets = asTomlTable(manifest.target);
	if (targets !== undefined) {
		for (const target of Object.values(targets)) {
			const table = asTomlTable(target);
			if (table !== undefined) collectDependencyGroups(table, memberSet, names, workspaceDependencies);
		}
	}
	return [...names].sort();
}

export function loadWorkspaceTopology(root: string): WorkspaceTopology {
	const problems: string[] = [];
	const rootManifestPath = join(root, "Cargo.toml");
	let rootManifest: string;
	try {
		rootManifest = readFileSync(rootManifestPath, "utf8");
	} catch {
		return {
			members: [],
			edges: [],
			problems: [`workspace manifest ${rootManifestPath} is not readable`],
		};
	}
	const members = parseWorkspaceMembers(rootManifest);
	if (members.length === 0) problems.push("workspace manifest declares no members");
	const edges = new Set<string>();
	for (const crate of FIXED_WORKSPACE_MEMBERS) {
		const manifestPath = join(root, "crates", crate, "Cargo.toml");
		let manifest: string;
		try {
			manifest = readFileSync(manifestPath, "utf8");
		} catch {
			problems.push(`crate manifest ${manifestPath} is not readable`);
			continue;
		}
		for (const dependency of parseInternalDependencyNames(manifest, FIXED_WORKSPACE_MEMBERS, rootManifest)) {
			edges.add(`${crate} -> ${dependency}`);
		}
	}
	return { members: [...members].sort(), edges: [...edges].sort(), problems };
}

export function verifyWorkspaceTopology(topology: WorkspaceTopology): string[] {
	const violations = [...topology.problems];
	for (const member of topology.members) {
		if (!FIXED_WORKSPACE_MEMBERS.includes(member)) violations.push(`unexpected workspace member ${member}`);
	}
	for (const member of FIXED_WORKSPACE_MEMBERS) {
		if (!topology.members.includes(member)) violations.push(`missing workspace member ${member}`);
	}
	for (const edge of topology.edges) {
		if (!FIXED_INTERNAL_EDGES.includes(edge)) violations.push(`unexpected internal dependency edge ${edge}`);
	}
	for (const edge of FIXED_INTERNAL_EDGES) {
		if (!topology.edges.includes(edge)) violations.push(`missing internal dependency edge ${edge}`);
	}
	return violations;
}

// ============================================================================
// Witness 2: forbidden crate references
// ============================================================================

export interface SourceFile {
	readonly crate: string;
	readonly path: string;
	readonly content: string;
}

export interface ForbiddenReferencePattern {
	readonly pattern: RegExp;
	readonly label: string;
}

/** The ledger's negative ownership imports. Cargo edges catch path-only use. */
export const FORBIDDEN_CRATE_REFERENCES: Readonly<Record<string, readonly ForbiddenReferencePattern[]>> = {
	"pi-agent": [
		{ pattern: /^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+pi_ext(?:\s*::|\s*;)/gm, label: "pi_ext" },
		{ pattern: /^\s*extern\s+crate\s+pi_ext\s*;/gm, label: "pi_ext" },
		{ pattern: /^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+pi_tui(?:\s*::|\s*;)/gm, label: "pi_tui" },
		{ pattern: /^\s*extern\s+crate\s+pi_tui\s*;/gm, label: "pi_tui" },
	],
	"pi-ext": [
		{ pattern: /^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+pi(?:\s*::|\s*;)/gm, label: "pi::" },
		{ pattern: /^\s*extern\s+crate\s+pi\s*;/gm, label: "pi::" },
	],
};

function maskRustNonCode(source: string): string {
	const code = [...source];
	let blockDepth = 0;
	let quoted = false;
	let escaped = false;
	let rawEnd: string | undefined;
	for (let index = 0; index < source.length; index += 1) {
		const character = source[index] ?? "";
		if (rawEnd !== undefined) {
			if (source.startsWith(rawEnd, index)) {
				code.fill(" ", index, index + rawEnd.length);
				index += rawEnd.length - 1;
				rawEnd = undefined;
			} else if (character !== "\n") code[index] = " ";
			continue;
		}
		if (blockDepth > 0) {
			if (source.startsWith("/*", index)) {
				blockDepth += 1;
				code.fill(" ", index, index + 2);
				index += 1;
			} else if (source.startsWith("*/", index)) {
				blockDepth -= 1;
				code.fill(" ", index, index + 2);
				index += 1;
			} else if (character !== "\n") code[index] = " ";
			continue;
		}
		if (quoted) {
			if (escaped) escaped = false;
			else if (character === "\\") escaped = true;
			else if (character === '"') quoted = false;
			if (character !== "\n") code[index] = " ";
			continue;
		}
		if (source.startsWith("//", index)) {
			while (index < source.length && source[index] !== "\n") {
				code[index] = " ";
				index += 1;
			}
			index -= 1;
			continue;
		}
		if (source.startsWith("/*", index)) {
			blockDepth = 1;
			code.fill(" ", index, index + 2);
			index += 1;
			continue;
		}
		const raw = source.slice(index).match(/^(?:br|cr|r)(#*)"/);
		if (raw !== null) {
			rawEnd = `"${raw[1] ?? ""}`;
			code.fill(" ", index, index + raw[0].length);
			index += raw[0].length - 1;
			continue;
		}
		if (character === '"') {
			quoted = true;
			code[index] = " ";
		}
	}
	return code.join("");
}

function scanSourceFile(file: SourceFile): string[] {
	const violations: string[] = [];
	const patterns = FORBIDDEN_CRATE_REFERENCES[file.crate] ?? [];
	const code = maskRustNonCode(file.content);
	for (const { pattern, label } of patterns) {
		pattern.lastIndex = 0;
		const match = pattern.exec(code);
		if (match === null) continue;
		const line = code.slice(0, match.index).split("\n").length;
		violations.push(`${file.path}:${line} references forbidden crate ${label}`);
	}
	return violations;
}

export function verifyCrateBoundaries(files: readonly SourceFile[]): string[] {
	return files.flatMap((file) => scanSourceFile(file));
}

// ============================================================================
// Shared markdown table parsing
// ============================================================================

interface TableRow {
	readonly lineNumber: number;
	readonly cells: readonly string[];
}

interface ParsedTable {
	readonly rows: readonly TableRow[];
	readonly problems: readonly string[];
}

function extractSection(text: string, heading: string): string {
	const marker = `## ${heading}`;
	const start = text.indexOf(marker);
	if (start === -1) return "";
	const rest = text.indexOf("\n## ", start + marker.length);
	return rest === -1 ? text.slice(start) : text.slice(start, rest);
}

function splitRowCells(line: string): string[] {
	return line
		.replace(/\r$/, "")
		.split("|")
		.slice(1, -1)
		.map((cell) => cell.trim());
}

function isSeparatorRow(cells: readonly string[]): boolean {
	return cells.length > 0 && cells.every((cell) => /^-{3,}$/.test(cell));
}

function parseMarkdownTable(section: string, headingLabel: string, expectedHeader: readonly string[]): ParsedTable {
	const problems: string[] = [];
	if (section === "") return { rows: [], problems: [`${headingLabel} section is missing from the ledger`] };
	const rows: TableRow[] = [];
	let headerSeen = false;
	const lines = section.split("\n");
	for (let index = 0; index < lines.length; index += 1) {
		const line = lines[index] ?? "";
		if (!line.startsWith("|")) continue;
		const cells = splitRowCells(line);
		if (isSeparatorRow(cells)) continue;
		if (!headerSeen) {
			headerSeen = true;
			expectedHeader.forEach((expected, columnIndex) => {
				const actual = cells[columnIndex] ?? "";
				if (actual !== expected) {
					problems.push(`${headingLabel} header cell ${columnIndex + 1} is "${actual}", expected "${expected}"`);
				}
			});
			continue;
		}
		if (cells.length !== expectedHeader.length) {
			problems.push(
				`${headingLabel} row at line ${index + 1} has ${cells.length} cells, expected ${expectedHeader.length}`,
			);
			continue;
		}
		rows.push({ lineNumber: index + 1, cells });
	}
	if (!headerSeen) problems.push(`${headingLabel} section contains no table header`);
	return { rows, problems };
}

// ============================================================================
// Witness 3: capability ledger
// ============================================================================

const CAPABILITY_FAMILIES: Readonly<Record<string, number>> = { A: 11, G: 11, T: 9, E: 4, C: 18, R: 4 };

const CAPABILITY_ID_RE = /^[A-Z]{1,2}\d{1,2}$/;

const LEDGER_STATUSES: readonly string[] = [
	"landed",
	"folded",
	"planned",
	"host-owned",
	"dev-only",
	"parity-blocked",
	"extension-plan-owned",
];

const A8_CHECKLIST_TOKENS: readonly string[] = [
	"upstream export map",
	"upstream source surface",
	"downstream importer corpus",
	"extension-host routing",
	"executable negative witnesses",
];

const SPECIAL_STATUS_REQUIREMENTS: Readonly<Record<string, string>> = {
	A8: "parity-blocked",
	E1: "extension-plan-owned",
};

const SPECIAL_OWNER_REQUIREMENTS: Readonly<Record<string, string>> = {
	A9: "pi-ai",
};

const SPECIAL_ROW_TOKENS: Readonly<Record<string, readonly string[]>> = {
	A9: ["OAuth-CLI"],
	E1: ["mirror/fixture lockstep witness"],
	R1: ["compiles on every target"],
	R2: ["compiles on every target"],
	R3: ["#[cfg(unix)]", "EndpointSpecError::UnsupportedOnPlatform", "portable"],
	R4: ["#[cfg(unix)]", "Windows-target compile check", "transport-neutral"],
};

export function expectedCapabilityIds(): string[] {
	const ids: string[] = [];
	for (const [family, count] of Object.entries(CAPABILITY_FAMILIES)) {
		for (let index = 1; index <= count; index += 1) ids.push(`${family}${index}`);
	}
	return ids;
}

interface CapabilityRow {
	readonly lineNumber: number;
	readonly id: string;
	readonly owner: string;
	readonly status: string;
	readonly rowText: string;
}

function parseCapabilityRows(ledgerText: string): { rows: readonly CapabilityRow[]; problems: readonly string[] } {
	const table = parseMarkdownTable(
		extractSection(ledgerText, "Capability ledger"),
		"Capability ledger",
		["ID", "Capability", "Owner", "Module", "Seam", "Status", "Evidence or contract"],
	);
	return {
		problems: table.problems,
		rows: table.rows.map((row) => ({
			lineNumber: row.lineNumber,
			id: row.cells[0] ?? "",
			owner: row.cells[2] ?? "",
			status: row.cells[5] ?? "",
			rowText: row.cells.join(" | "),
		})),
	};
}

export function verifyCapabilityLedger(ledgerText: string): string[] {
	const { rows, problems } = parseCapabilityRows(ledgerText);
	const violations = [...problems];
	const expected = expectedCapabilityIds();
	const seen = new Set<string>();
	const duplicates = new Set<string>();
	for (const row of rows) {
		if (!CAPABILITY_ID_RE.test(row.id)) {
			violations.push(`capability row at line ${row.lineNumber} has malformed ID "${row.id}"`);
			continue;
		}
		if (seen.has(row.id)) duplicates.add(row.id);
		seen.add(row.id);
		if (!FIXED_WORKSPACE_MEMBERS.includes(row.owner)) {
			violations.push(`capability ${row.id} owner "${row.owner}" is not a workspace crate`);
		}
		if (!LEDGER_STATUSES.includes(row.status)) {
			violations.push(`capability ${row.id} status "${row.status}" is not a ledger status`);
		}
		const requiredStatus = SPECIAL_STATUS_REQUIREMENTS[row.id];
		if (requiredStatus !== undefined && row.status !== requiredStatus) {
			violations.push(`capability ${row.id} status must be "${requiredStatus}", found "${row.status}"`);
		}
		const requiredOwner = SPECIAL_OWNER_REQUIREMENTS[row.id];
		if (requiredOwner !== undefined && row.owner !== requiredOwner) {
			violations.push(`capability ${row.id} owner must be "${requiredOwner}", found "${row.owner}"`);
		}
		for (const token of SPECIAL_ROW_TOKENS[row.id] ?? []) {
			if (!row.rowText.includes(token)) {
				violations.push(`capability ${row.id} is missing pinned contract token "${token}"`);
			}
		}
		if (row.id === "A8") {
			for (const token of A8_CHECKLIST_TOKENS) {
				if (!row.rowText.includes(token)) {
					violations.push(`capability A8 evidence checklist is missing "${token}"`);
				}
			}
		}
	}
	for (const id of duplicates) violations.push(`duplicate capability ID ${id}`);
	if (rows.length !== expected.length) {
		violations.push(`capability ledger has ${rows.length} rows, expected ${expected.length}`);
	}
	for (const id of expected) {
		if (!seen.has(id)) violations.push(`missing capability ID ${id}`);
	}
	for (const id of seen) {
		if (!expected.includes(id)) violations.push(`unexpected capability ID ${id}`);
	}
	return violations;
}

// ============================================================================
// Witness 4: graduated ticket DAG
// ============================================================================

const STABLE_ID_RE = /^[A-Z]+(?:-[A-Z0-9]+)+$/;

export interface GraduatedTicketPin {
	readonly id: string;
	readonly blockedBy: readonly string[];
}

/** Exact membership and edges make coherent row deletion observable. */
export const GRADUATED_TICKET_PINS: readonly GraduatedTicketPin[] = [
	{ id: "PAR-LEDGER", blockedBy: [] },
	{ id: "PAR-TEL", blockedBy: ["PAR-LEDGER"] },
	{ id: "PAR-CLI-PROTO", blockedBy: ["PAR-LEDGER"] },
	{ id: "PAR-CLI", blockedBy: ["PAR-CLI-PROTO"] },
	{ id: "PAR-WIRE", blockedBy: ["PAR-LEDGER"] },
	{ id: "PAR-CODEC", blockedBy: ["PAR-WIRE"] },
	{ id: "PAR-CLIENT", blockedBy: ["PAR-CODEC"] },
	{ id: "PAR-SERVER", blockedBy: ["PAR-CLIENT"] },
	{ id: "PAR-COMPAT-AUDIT", blockedBy: ["PAR-LEDGER"] },
	{ id: "PAR-COMPAT-DISPO", blockedBy: ["PAR-COMPAT-AUDIT"] },
	{ id: "PAR-MATH-RESEARCH", blockedBy: ["PAR-LEDGER"] },
	{ id: "PAR-MATH", blockedBy: ["PAR-MATH-RESEARCH"] },
	{ id: "PAR-FOLD", blockedBy: ["PAR-TEL", "PAR-CLI", "PAR-COMPAT-DISPO"] },
	{ id: "PAR-PTY-GRILL", blockedBy: ["PAR-LEDGER", "PAR-MATH"] },
	{ id: "XC-2", blockedBy: [] },
	{
		id: "PAR-CLOSE",
		blockedBy: [
			"PAR-FOLD",
			"PAR-CLIENT",
			"PAR-SERVER",
			"PAR-COMPAT-AUDIT",
			"PAR-COMPAT-DISPO",
			"PAR-PTY-GRILL",
			"XC-2",
		],
	},
];

interface TicketRow {
	readonly lineNumber: number;
	readonly id: string;
	readonly blockedBy: readonly string[];
}

function parseTicketRows(ledgerText: string): { rows: readonly TicketRow[]; problems: readonly string[] } {
	const table = parseMarkdownTable(
		extractSection(ledgerText, "Graduated parity-ticket DAG"),
		"Graduated parity-ticket DAG",
		["Stable ID", "Kind", "blocked_by"],
	);
	return {
		problems: table.problems,
		rows: table.rows.map((row) => {
			const blockedCell = row.cells[2] ?? "";
			return {
				lineNumber: row.lineNumber,
				id: row.cells[0] ?? "",
				blockedBy:
					blockedCell === "—"
						? []
						: blockedCell
								.split(",")
								.map((entry) => entry.trim())
								.filter((entry) => entry !== ""),
			};
		}),
	};
}

export function verifyGraduatedTicketDag(ledgerText: string): string[] {
	const { rows, problems } = parseTicketRows(ledgerText);
	const violations = [...problems];
	const ids = new Set<string>();
	const duplicates = new Set<string>();
	const validRows: TicketRow[] = [];
	for (const row of rows) {
		if (!STABLE_ID_RE.test(row.id)) {
			violations.push(`ticket row at line ${row.lineNumber} has malformed stable ID "${row.id}"`);
			continue;
		}
		if (ids.has(row.id)) duplicates.add(row.id);
		ids.add(row.id);
		validRows.push(row);
	}
	for (const id of duplicates) violations.push(`duplicate ticket stable ID ${id}`);
	for (const row of validRows) {
		for (const blocker of row.blockedBy) {
			if (!ids.has(blocker)) {
				violations.push(`ticket ${row.id} blocked_by "${blocker}" has no row (missing node)`);
			}
		}
	}
	// Kahn's algorithm: unresolved nodes after topological processing are cyclic.
	const blockedCount = new Map<string, number>([...ids].map((id) => [id, 0]));
	const dependentsOf = new Map<string, string[]>([...ids].map((id) => [id, []]));
	for (const row of validRows) {
		for (const blocker of row.blockedBy) {
			if (!ids.has(blocker)) continue;
			blockedCount.set(row.id, (blockedCount.get(row.id) ?? 0) + 1);
			dependentsOf.get(blocker)?.push(row.id);
		}
	}
	const resolved = new Set<string>();
	const pending = [...ids].filter((id) => (blockedCount.get(id) ?? 0) === 0);
	while (pending.length > 0) {
		const id = pending.pop();
		if (id === undefined || resolved.has(id)) continue;
		resolved.add(id);
		for (const dependent of dependentsOf.get(id) ?? []) {
			const remaining = (blockedCount.get(dependent) ?? 0) - 1;
			blockedCount.set(dependent, remaining);
			if (remaining === 0) pending.push(dependent);
		}
	}
	if (resolved.size !== ids.size) {
		const cyclic = [...ids].filter((id) => !resolved.has(id)).sort();
		violations.push(`ticket DAG contains a dependency cycle involving: ${cyclic.join(", ")}`);
	}
	const pinById: Readonly<Record<string, GraduatedTicketPin>> = Object.fromEntries(
		GRADUATED_TICKET_PINS.map((pin) => [pin.id, pin]),
	);
	for (const pin of GRADUATED_TICKET_PINS) {
		if (!ids.has(pin.id)) violations.push(`missing graduated ticket ${pin.id}`);
	}
	for (const row of validRows) {
		const pin = pinById[row.id];
		if (pin === undefined) {
			violations.push(`unexpected graduated ticket ${row.id}`);
			continue;
		}
		const actual = [...row.blockedBy].sort().join(", ");
		const expected = [...pin.blockedBy].sort().join(", ");
		if (actual !== expected) {
			violations.push(`ticket ${row.id} blocked_by mismatch (expected ${expected}; found ${actual})`);
		}
	}
	return violations;
}

// ============================================================================
// Witness 5: AgentLoopConfig literal sites
// ============================================================================

export interface AgentLoopConfigSite {
	readonly path: string;
	readonly line: number;
}

const AGENT_LOOP_CONFIG_LITERAL_RE = /\bAgentLoopConfig\s*\{/g;

/**
 * Literal construction sites only: a match preceded by `->`, `struct`, or
 * `impl` is a signature or declaration, not an arbitration site.
 */
export function enumerateAgentLoopConfigSites(sourceFiles: Readonly<Record<string, string>>): AgentLoopConfigSite[] {
	const sites: AgentLoopConfigSite[] = [];
	for (const [path, content] of Object.entries(sourceFiles)) {
		const lines = maskRustNonCode(content).split("\n");
		for (let index = 0; index < lines.length; index += 1) {
			const line = lines[index] ?? "";
			AGENT_LOOP_CONFIG_LITERAL_RE.lastIndex = 0;
			let match = AGENT_LOOP_CONFIG_LITERAL_RE.exec(line);
			while (match !== null) {
				const prefix = line.slice(0, match.index).trimEnd();
				const isDeclaration = prefix.endsWith("->") || /\b(?:struct|impl)$/.test(prefix);
				if (!isDeclaration) sites.push({ path, line: index + 1 });
				match = AGENT_LOOP_CONFIG_LITERAL_RE.exec(line);
			}
		}
	}
	return sites.sort((a, b) => (a.path === b.path ? a.line - b.line : a.path < b.path ? -1 : 1));
}

export function verifyAgentLoopConfigSites(sites: readonly AgentLoopConfigSite[]): string[] {
	const violations: string[] = [];
	if (sites.length !== PINNED_AGENT_LOOP_CONFIG_SITES.length) {
		violations.push(
			`expected exactly ${PINNED_AGENT_LOOP_CONFIG_SITES.length} AgentLoopConfig literal sites, found ${sites.length}`,
		);
	}
	const pinnedList = PINNED_AGENT_LOOP_CONFIG_SITES.map((pin) => `${pin.path}:${pin.start}-${pin.end}`).join(", ");
	const consumed = new Set<number>();
	for (const site of sites) {
		const pinIndex = PINNED_AGENT_LOOP_CONFIG_SITES.findIndex(
			(pin) => pin.path === site.path && site.line >= pin.start && site.line <= pin.end,
		);
		if (pinIndex === -1) {
			violations.push(`AgentLoopConfig literal at ${site.path}:${site.line} matches no pinned site (${pinnedList})`);
			continue;
		}
		if (consumed.has(pinIndex)) {
			const pin = PINNED_AGENT_LOOP_CONFIG_SITES[pinIndex];
			violations.push(`multiple AgentLoopConfig literals inside pinned range ${pin?.path}:${pin?.start}-${pin?.end}`);
		}
		consumed.add(pinIndex);
	}
	PINNED_AGENT_LOOP_CONFIG_SITES.forEach((pin, index) => {
		if (!consumed.has(index)) violations.push(`missing pinned AgentLoopConfig literal site ${pin.path}:${pin.start}-${pin.end}`);
	});
	return violations;
}

// ============================================================================
// Repository collection and orchestration
// ============================================================================

function listSourceFiles(root: string, directory: string): string[] {
	const results: string[] = [];
	const stack: string[] = [directory];
	while (stack.length > 0) {
		const current = stack.pop();
		if (current === undefined) continue;
		let entries: Dirent[];
		try {
			entries = readdirSync(join(root, current), { withFileTypes: true });
		} catch {
			continue;
		}
		for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
			const relative = `${current}/${entry.name}`;
			if (entry.isDirectory()) stack.push(relative);
			else if (entry.isFile() && entry.name.endsWith(".rs")) results.push(relative);
		}
	}
	return results.sort();
}

export interface CollectedSources {
	readonly files: readonly SourceFile[];
	readonly problems: readonly string[];
}

/** Boundary sources for every crate named in FORBIDDEN_CRATE_REFERENCES. */
export function collectBoundarySourceFiles(root: string): CollectedSources {
	const files: SourceFile[] = [];
	const problems: string[] = [];
	for (const crate of Object.keys(FORBIDDEN_CRATE_REFERENCES)) {
		const directory = `crates/${crate}/src`;
		const paths = listSourceFiles(root, directory);
		if (paths.length === 0) problems.push(`${directory} contains no Rust sources`);
		for (const path of paths) {
			files.push({ crate, path, content: readFileSync(join(root, path), "utf8") });
		}
	}
	return { files, problems };
}

/** Every Rust source under crates/, the universe the site oracle enumerates. */
export function collectRustSources(root: string): { files: Readonly<Record<string, string>>; problems: readonly string[] } {
	const files: Record<string, string> = {};
	for (const path of listSourceFiles(root, "crates")) {
		files[path] = readFileSync(join(root, path), "utf8");
	}
	const problems: string[] = [];
	if (Object.keys(files).length === 0) problems.push("crates/ contains no Rust sources");
	return { files, problems };
}

/** Run all five witnesses against one repository root; empty means green. */
export function runParityWitnesses(root: string): string[] {
	const violations: string[] = [];
	const add = (witness: string, results: readonly string[]): void => {
		for (const result of results) violations.push(`[${witness}] ${result}`);
	};

	let ledgerText: string | null = null;
	try {
		ledgerText = readFileSync(join(root, "docs/PARITY_LEDGER.md"), "utf8");
	} catch {
		add("ledger", ["docs/PARITY_LEDGER.md is not readable"]);
	}

	add("workspace-topology", verifyWorkspaceTopology(loadWorkspaceTopology(root)));

	const boundary = collectBoundarySourceFiles(root);
	add("crate-boundaries", [...boundary.problems, ...verifyCrateBoundaries(boundary.files)]);

	if (ledgerText !== null) {
		add("capability-ledger", verifyCapabilityLedger(ledgerText));
		add("graduated-dag", verifyGraduatedTicketDag(ledgerText));
	}

	const sources = collectRustSources(root);
	add(
		"agent-loop-config-sites",
		[...sources.problems, ...verifyAgentLoopConfigSites(enumerateAgentLoopConfigSites(sources.files))],
	);

	return violations;
}

function main(): void {
	const violations = runParityWitnesses(REPO_ROOT);
	if (violations.length > 0) {
		console.error(`parity witness suite failed with ${violations.length} violation(s):`);
		for (const violation of violations) console.error(`  - ${violation}`);
		process.exit(1);
	}
	process.stdout.write("PARITY_WITNESSES_OK\n");
}

if (import.meta.main) main();
