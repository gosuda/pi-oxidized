#!/usr/bin/env bun
/**
 * Compatibility acceptance matrix orchestrator.
 *
 * Reads scripts/verification/compat-matrix.json, validates the schema, runs
 * the selected acceptance rows, and writes a machine-readable result artifact.
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { arch, cpus, hostname, platform, release } from "node:os";
import { dirname, resolve } from "node:path";

const TIER_VALUES = ["unit", "host", "product", "performance", "release"] as const;
type Tier = (typeof TIER_VALUES)[number];

const TIER_STRINGS: readonly string[] = TIER_VALUES;

function isTier(value: string): value is Tier {
	return TIER_STRINGS.includes(value);
}

interface CommandSpec {
	readonly cwd: string;
	readonly argv: readonly string[];
	readonly timeoutMs: number;
}

interface MatrixRow {
	readonly id: string;
	readonly surface: string;
	readonly tier: Tier;
	readonly required: boolean;
	readonly excluded?: boolean;
	readonly requires?: readonly string[];
	readonly commands: readonly CommandSpec[];
	readonly evidence: string;
	readonly rationale?: string;
	readonly citation?: string;
}

export interface Matrix {
	readonly version?: string;
	readonly rows: readonly MatrixRow[];
}

interface CommandResult {
	readonly key: string;
	readonly cwd: string;
	readonly argv: readonly string[];
	readonly exitCode: number | null;
	readonly durationMs: number;
	readonly timedOut: boolean;
	readonly stdoutTail: string;
	readonly stderrTail: string;
	readonly rowIds: readonly string[];
}

interface RowResult {
	readonly rowId: string;
	readonly status: "passed" | "failed" | "skipped" | "dry-run";
	readonly exitCode?: number | null;
	readonly durationMs?: number;
	readonly commandKey?: string;
	readonly error?: string;
}

interface MatrixResult {
	readonly version: string;
	readonly matrixPath: string;
	readonly startedAt: string;
	readonly finishedAt: string;
	readonly dryRun: boolean;
	readonly gitHead: string | undefined;
	readonly bun: { readonly version: string };
	readonly machine: {
		readonly platform: string;
		readonly arch: string;
		readonly hostname: string;
		readonly cpuModel: string | undefined;
		readonly osRelease: string;
	};
	readonly selection: {
		readonly rowIds: readonly string[];
		readonly tier: Tier | undefined;
	};
	readonly commandResults: CommandResult[];
	readonly rowResults: RowResult[];
	readonly summary: {
		readonly total: number;
		readonly passed: number;
		readonly failed: number;
		readonly skipped: number;
		readonly requiredFailed: readonly string[];
	};
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isString(value: unknown): value is string {
	return typeof value === "string";
}

function isBoolean(value: unknown): value is boolean {
	return typeof value === "boolean";
}

function isNumber(value: unknown): value is number {
	return typeof value === "number" && Number.isFinite(value);
}

function isStringArray(value: unknown): value is string[] {
	return Array.isArray(value) && value.every(isString);
}

function isCommandSpec(value: unknown): value is CommandSpec {
	if (!isPlainObject(value)) return false;
	const cwd = value.cwd;
	const argv = value.argv;
	const timeoutMs = value.timeoutMs;
	return (
		isString(cwd) &&
		isStringArray(argv) &&
		argv.length > 0 &&
		isNumber(timeoutMs) &&
		timeoutMs > 0
	);
}

function assertCommandSpec(value: unknown, path: string): asserts value is CommandSpec {
	if (!isPlainObject(value)) {
		throw new Error(`${path} must be an object`);
	}
	if (!isString(value.cwd)) throw new Error(`${path}.cwd must be a string`);
	if (!isStringArray(value.argv) || value.argv.length === 0) {
		throw new Error(`${path}.argv must be a non-empty array of strings`);
	}
	if (!isNumber(value.timeoutMs) || value.timeoutMs <= 0) {
		throw new Error(`${path}.timeoutMs must be a positive number`);
	}
}

function assertMatrixRow(value: unknown, index: number): asserts value is MatrixRow {
	if (!isPlainObject(value)) {
		throw new Error(`rows[${index}] must be an object`);
	}
	const id = value.id;
	const surface = value.surface;
	const tier = value.tier;
	const required = value.required;
	if (!isString(id) || id.length === 0) {
		throw new Error(`rows[${index}].id must be a non-empty string`);
	}
	if (!isString(surface) || surface.length === 0) {
		throw new Error(`rows[${index}].surface must be a non-empty string`);
	}
	if (!isString(tier)) {
		throw new Error(`rows[${index}].tier must be a string`);
	}
	if (!isTier(tier)) {
		throw new Error(
			`rows[${index}].tier must be one of ${TIER_VALUES.join(", ")}`,
		);
	}
	if (!isBoolean(required)) {
		throw new Error(`rows[${index}].required must be a boolean`);
	}
	const commands = value.commands;
	if (!Array.isArray(commands)) {
		throw new Error(`rows[${index}].commands must be an array`);
	}
	for (let i = 0; i < commands.length; i++) {
		assertCommandSpec(commands[i], `rows[${index}].commands[${i}]`);
	}
	if (!isString(value.evidence) || value.evidence.length === 0) {
		throw new Error(`rows[${index}].evidence must be a non-empty string`);
	}
	if (value.excluded !== undefined && !isBoolean(value.excluded)) {
		throw new Error(`rows[${index}].excluded must be a boolean`);
	}
	if (value.requires !== undefined && !isStringArray(value.requires)) {
		throw new Error(`rows[${index}].requires must be a string array`);
	}
	if (value.rationale !== undefined && !isString(value.rationale)) {
		throw new Error(`rows[${index}].rationale must be a string`);
	}
	if (value.citation !== undefined && !isString(value.citation)) {
		throw new Error(`rows[${index}].citation must be a string`);
	}
	if (value.excluded && commands.length > 0) {
		throw new Error(
			`rows[${index}] is excluded and must not have commands`,
		);
	}
}

export function validateMatrix(value: unknown): Matrix {
	if (!isPlainObject(value)) {
		throw new Error("Matrix must be an object");
	}
	if (value.version !== undefined && !isString(value.version)) {
		throw new Error("version must be a string");
	}
	const rows = value.rows;
	if (!Array.isArray(rows)) {
		throw new Error("rows must be an array");
	}
	if (rows.length === 0) {
		throw new Error("matrix must contain at least one row");
	}
	const seen = new Set<string>();
	for (let i = 0; i < rows.length; i++) {
		const row = rows[i];
		assertMatrixRow(row, i);
		if (seen.has(row.id)) {
			throw new Error(`duplicate row id: ${row.id}`);
		}
		seen.add(row.id);
	}
	return { version: isString(value.version) ? value.version : undefined, rows } as Matrix;
}

export interface SelectionRequest {
	readonly rows?: readonly string[];
	readonly tier?: Tier;
}

export function selectRows(
	matrix: Matrix,
	request: SelectionRequest,
): readonly MatrixRow[] {
	const byRow = request.rows && request.rows.length > 0;
	const byTier = request.tier !== undefined;
	if (!byRow && !byTier) {
		return matrix.rows.filter((row) => row.required && !row.excluded);
	}
	const ids = new Set(request.rows ?? []);
	return matrix.rows.filter(
		(row) => ids.has(row.id) || (byTier && row.tier === request.tier),
	);
}

function commandKey(command: CommandSpec): string {
	return JSON.stringify({ cwd: command.cwd, argv: command.argv });
}

export interface GroupedCommand {
	readonly command: CommandSpec;
	readonly rowIds: readonly string[];
}

export function uniqueCommands(rows: readonly MatrixRow[]): GroupedCommand[] {
	const groups = new Map<string, GroupedCommand>();
	for (const row of rows) {
		if (row.excluded || row.commands.length === 0) continue;
		const rowCommand = row.commands[0];
		if (rowCommand === undefined) continue;
		if (row.commands.length > 1) {
			throw new Error(
				`row ${row.id} has more than one command; only single-command rows are supported`,
			);
		}
		const key = commandKey(rowCommand);
		const existing = groups.get(key);
		if (existing === undefined) {
			groups.set(key, { command: rowCommand, rowIds: [row.id] });
		} else {
			const nextIds = [...existing.rowIds, row.id];
			groups.set(key, { command: rowCommand, rowIds: nextIds });
		}
	}
	return [...groups.values()];
}

function tail(text: string, maximum: number): string {
	if (text.length <= maximum) return text;
	return text.slice(-maximum);
}

export interface SpawnResult {
	readonly exitCode: number | null;
	readonly durationMs: number;
	readonly timedOut: boolean;
	readonly stdout: string;
	readonly stderr: string;
	readonly killedBySignal?: string;
}

export async function runCommand(
	command: CommandSpec,
	repoRoot: string,
	abortSignal?: AbortSignal,
): Promise<SpawnResult> {
	const cwd = resolve(repoRoot, command.cwd);
	const started = performance.now();
	try {
		const proc = Bun.spawn({
			cmd: [...command.argv],
			cwd,
			stdout: "pipe",
			stderr: "pipe",
			signal: abortSignal,
		});

		const [stdout, stderr] = await Promise.all([
			new Response(proc.stdout).text(),
			new Response(proc.stderr).text(),
		]);

		const exitCode = await proc.exited;
		const durationMs = Math.round(performance.now() - started);
		return {
			exitCode,
			durationMs,
			timedOut: abortSignal !== undefined && abortSignal.aborted,
			stdout: tail(stdout, 12_000),
			stderr: tail(stderr, 12_000),
			killedBySignal: proc.signalCode ?? undefined,
		};
	} catch (error) {
		const durationMs = Math.round(performance.now() - started);
		const message = error instanceof Error ? error.message : String(error);
		return {
			exitCode: null,
			durationMs,
			timedOut: false,
			stdout: "",
			stderr: `launch/read error: ${message}`,
			killedBySignal: undefined,
		};
	}
}

function isoNow(): string {
	return new Date().toISOString();
}

async function gitHead(repoRoot: string): Promise<string | undefined> {
	try {
		const proc = Bun.spawn({
			cmd: ["git", "rev-parse", "HEAD"],
			cwd: repoRoot,
			stdout: "pipe",
			stderr: "pipe",
		});
		const out = await new Response(proc.stdout).text();
		const code = await proc.exited;
		if (code === 0) return out.trim();
		return undefined;
	} catch {
		return undefined;
	}
}

export async function runMatrix(options: {
	readonly matrix: Matrix;
	readonly matrixPath: string;
	readonly repoRoot: string;
	readonly request: SelectionRequest;
	readonly dryRun: boolean;
}): Promise<MatrixResult> {
	const { matrix, matrixPath, repoRoot, request, dryRun } = options;
	const selected = selectRows(matrix, request);

	// Check declared prerequisites before grouping commands so a missing
	// prerequisite fails the row with a named error instead of an opaque
	// command failure downstream.
	const prerequisiteFailures = new Map<string, string>();
	for (const row of selected) {
		if (row.excluded || row.requires === undefined) continue;
		for (const requiredPath of row.requires) {
			if (!existsSync(resolve(repoRoot, requiredPath))) {
				prerequisiteFailures.set(
					row.id,
					`missing prerequisite: ${requiredPath} not found under ${repoRoot}`,
				);
				break;
			}
		}
	}

	const runnableRows = selected.filter((row) => !prerequisiteFailures.has(row.id));
	const groups = uniqueCommands(runnableRows);
	const commandResults: CommandResult[] = [];
	const rowResultById = new Map<string, RowResult>();
	const startedAt = isoNow();

	for (const row of selected) {
		if (row.excluded) {
			rowResultById.set(row.id, {
				rowId: row.id,
				status: "skipped",
				error: row.rationale,
			});
			continue;
		}
		const prereqError = prerequisiteFailures.get(row.id);
		if (prereqError !== undefined) {
			rowResultById.set(row.id, {
				rowId: row.id,
				status: "failed",
				error: prereqError,
			});
			continue;
		}
		if (dryRun) {
			rowResultById.set(row.id, {
				rowId: row.id,
				status: "dry-run",
			});
		}
	}

	if (!dryRun) {
		for (const group of groups) {
			const timeoutController = new AbortController();
			let timeoutId: NodeJS.Timeout | undefined;
			if (group.command.timeoutMs > 0) {
				timeoutId = setTimeout(() => timeoutController.abort(), group.command.timeoutMs);
			}
			let spawnResult: SpawnResult;
			try {
				spawnResult = await runCommand(
					group.command,
					repoRoot,
					timeoutController.signal,
				);
			} finally {
				clearTimeout(timeoutId);
			}

			const result: CommandResult = {
				key: commandKey(group.command),
				cwd: group.command.cwd,
				argv: group.command.argv,
				exitCode: spawnResult.exitCode,
				durationMs: spawnResult.durationMs,
				timedOut: spawnResult.timedOut,
				stdoutTail: spawnResult.stdout,
				stderrTail: spawnResult.stderr,
				rowIds: group.rowIds,
			};
			commandResults.push(result);

			for (const rowId of group.rowIds) {
				const passed = spawnResult.exitCode === 0 && !spawnResult.timedOut;
				rowResultById.set(rowId, {
					rowId,
					status: passed ? "passed" : "failed",
					exitCode: spawnResult.exitCode,
					durationMs: spawnResult.durationMs,
					commandKey: result.key,
					error: spawnResult.timedOut
						? `timed out after ${group.command.timeoutMs}ms`
						: spawnResult.exitCode === null
							? spawnResult.stderr
							: spawnResult.exitCode !== 0
								? `exit code ${spawnResult.exitCode}`
								: undefined,
				});
			}
		}
	}

	const rowResults: RowResult[] = selected.map((row) => {
		const result = rowResultById.get(row.id);
		if (result === undefined) {
			return { rowId: row.id, status: "skipped", error: "no result recorded" };
		}
		return result;
	});

	const requiredFailed = rowResults
		.filter((r) => {
			const row = matrix.rows.find((x) => x.id === r.rowId);
			return row !== undefined && row.required && r.status === "failed";
		})
		.map((r) => r.rowId);

	const summary = {
		total: rowResults.length,
		passed: rowResults.filter((r) => r.status === "passed").length,
		failed: rowResults.filter((r) => r.status === "failed").length,
		skipped: rowResults.filter((r) => r.status === "skipped").length,
		requiredFailed,
	};

	return {
		version: matrix.version ?? "unknown",
		matrixPath,
		startedAt,
		finishedAt: isoNow(),
		dryRun,
		gitHead: await gitHead(repoRoot),
		bun: { version: Bun.version },
		machine: {
			platform: platform(),
			arch: arch(),
			hostname: hostname(),
			cpuModel: cpus()[0]?.model,
			osRelease: release(),
		},
		selection: {
			rowIds: selected.map((row) => row.id),
			tier: request.tier,
		},
		commandResults,
		rowResults,
		summary,
	};
}

interface CliOptions {
	readonly matrixPath: string;
	readonly rows: string[];
	readonly tier: Tier | undefined;
	readonly dryRun: boolean;
}

function parseArgv(argv: readonly string[]): CliOptions {
	const rows: string[] = [];
	let tier: Tier | undefined;
	let dryRun = false;
	let matrixPath: string | undefined;

	for (let i = 0; i < argv.length; i++) {
		const arg = argv[i];
		if (arg === undefined) continue;
		switch (arg) {
			case "--row": {
				const next = argv[i + 1];
				if (next === undefined || next.startsWith("-")) {
					throw new Error("--row requires a value");
				}
				rows.push(next);
				i += 1;
				break;
			}
			case "--tier": {
				const next = argv[i + 1];
				if (next === undefined || next.startsWith("-")) {
					throw new Error("--tier requires a value");
				}
				if (!isTier(next)) {
					throw new Error(
						`--tier must be one of ${TIER_VALUES.join(", ")}`,
					);
				}
				tier = next;
				i += 1;
				break;
			}
			case "--dry-run": {
				dryRun = true;
				break;
			}
			case "--matrix": {
				const next = argv[i + 1];
				if (next === undefined || next.startsWith("-")) {
					throw new Error("--matrix requires a value");
				}
				matrixPath = next;
				i += 1;
				break;
			}
			case "-h":
			case "--help": {
				process.stdout.write(
					"usage: compat-matrix.ts [--row ID]... [--tier TIER] [--dry-run] [--matrix PATH]\n",
				);
				process.exit(0);
				break;
			}
			default: {
				if (arg.startsWith("-")) {
					throw new Error(`Unknown argument: ${arg}`);
				}
				throw new Error(`Unexpected positional argument: ${arg}`);
			}
		}
	}

	return {
		matrixPath: matrixPath ?? "scripts/verification/compat-matrix.json",
		rows,
		tier,
		dryRun,
	};
}

async function main(): Promise<void> {
	const repoRoot = resolve(import.meta.dirname, "../..");
	const options = parseArgv(process.argv.slice(2));
	const matrixPath = resolve(repoRoot, options.matrixPath);

	let raw: string;
	try {
		raw = readFileSync(matrixPath, "utf8");
	} catch (error) {
		console.error(`Cannot read matrix at ${options.matrixPath}: ${error}`);
		process.exit(2);
	}

	let parsed: unknown;
	try {
		parsed = JSON.parse(raw);
	} catch (error) {
		console.error(`Invalid JSON in ${options.matrixPath}: ${error}`);
		process.exit(2);
	}

	const matrix = validateMatrix(parsed);

	const request: SelectionRequest = {
		rows: options.rows.length > 0 ? options.rows : undefined,
		tier: options.tier,
	};

	const unknownRows = options.rows.filter(
		(id) => !matrix.rows.some((row) => row.id === id),
	);
	if (unknownRows.length > 0) {
		console.error(`Unknown row id(s): ${unknownRows.join(", ")}`);
		process.exit(2);
	}

	const result = await runMatrix({
		matrix,
		matrixPath: options.matrixPath,
		repoRoot,
		request,
		dryRun: options.dryRun,
	});

	const outDir = resolve(repoRoot, "target/verification/compat-matrix");
	mkdirSync(outDir, { recursive: true });
	const outPath = resolve(outDir, "result.json");
	writeFileSync(outPath, JSON.stringify(result, null, "\t"), "utf8");

	if (result.summary.requiredFailed.length > 0) {
		console.error(
			`Compatibility matrix failed required rows: ${result.summary.requiredFailed.join(", ")}`,
		);
		process.exit(1);
	}

	if (options.dryRun) {
		process.stdout.write(
			`Dry-run wrote ${result.summary.total} selected rows to ${outPath}\n`,
		);
	} else {
		process.stdout.write(
			`Compatibility matrix passed (${result.summary.passed}/${result.summary.total}; ${result.summary.skipped} skipped) -> ${outPath}\n`,
		);
	}
}

if (import.meta.main) {
	await main().catch((error: unknown) => {
		console.error(error);
		process.exit(2);
	});
}
