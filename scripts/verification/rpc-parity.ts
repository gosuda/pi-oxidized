#!/usr/bin/env bun
/**
 * Full cross-binary RPC parity replay (verification check 10).
 *
 * Drives every authoritative RPC command through the Rust release binary
 * (`pi --mode rpc`) and the source-pinned TypeScript pi (`cli.ts --mode rpc`)
 * as real processes speaking JSONL on stdin/stdout, then compares the
 * normalized response/event transcripts.
 *
 * Command coverage is derived from the authoritative `RpcCommand` union in
 * `.references/pi/packages/coding-agent/src/modes/rpc/rpc-types.ts`; a newly
 * added command fails the coverage assertion instead of silently escaping.
 *
 * Normalization is limited to generated identifiers (UUIDs and 8-hex entry
 * ids, mapped in first-seen order so referential structure is preserved),
 * timestamps/elapsed-time values, and per-run temporary paths.
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

export const REPO_ROOT = resolve(import.meta.dirname, "../..");
const RUST_BINARY = resolve(REPO_ROOT, "target/release/pi");
const EXTENSION_HOST = resolve(REPO_ROOT, "packages/extension-host/dist/pi-extension-host");
const EXTENSION_PATH = resolve(import.meta.dirname, "extension.ts");
const TYPESCRIPT_CLI = resolve(REPO_ROOT, ".references/pi/packages/coding-agent/src/cli.ts");
export const AUTHORITATIVE_RPC_TYPES_PATH = resolve(
	REPO_ROOT,
	".references/pi/packages/coding-agent/src/modes/rpc/rpc-types.ts",
);
const EVIDENCE_ROOT = resolve(REPO_ROOT, "target/verification/rpc-parity");

const VERIFICATION_PROVIDER = "verification";
const VERIFICATION_MODEL = "model";
const TOOL_FILE = "verification-rpc.txt";
const STEP_DEADLINE_MS = Number(process.env.PI_RPC_PARITY_STEP_TIMEOUT_MS ?? "120000");
const EXIT_DEADLINE_MS = 30_000;

const RUST_RELEASE_BUILD_COMMAND = ["cargo", "build", "-p", "pi", "--release", "--locked"];

interface JsonObject {
	[key: string]: JsonValue;
}
type JsonValue = null | boolean | number | string | JsonValue[] | JsonObject;

function fail(message: string): never {
	throw new Error(message);
}

function assert(condition: boolean, message: string): asserts condition {
	if (!condition) fail(message);
}

function isObject(value: JsonValue | undefined): value is JsonObject {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

// ============================================================================
// Authoritative command coverage
// ============================================================================

/**
 * Extract every `type: "..."` discriminant from the authoritative
 * `RpcCommand` union. Scans from the declaration to the first top-level `;`
 * with brace-depth tracking so property separators inside variants are not
 * mistaken for the union terminator.
 */
export function deriveRpcCommandTypes(source: string): string[] {
	const marker = "export type RpcCommand =";
	const start = source.indexOf(marker);
	assert(start !== -1, `authoritative RpcCommand union not found (looked for ${JSON.stringify(marker)})`);
	let depth = 0;
	let end = -1;
	for (let index = start + marker.length; index < source.length; index++) {
		const char = source[index];
		if (char === "{") depth++;
		else if (char === "}") depth--;
		else if (char === ";" && depth === 0) {
			end = index;
			break;
		}
	}
	assert(end !== -1, "authoritative RpcCommand union has no top-level terminator");
	const block = source.slice(start + marker.length, end);
	const types: string[] = [];
	for (const match of block.matchAll(/type:\s*"([a-z0-9_]+)"/g)) {
		const type = match[1];
		if (type !== undefined && !types.includes(type)) types.push(type);
	}
	assert(types.length > 0, "authoritative RpcCommand union yielded no discriminants");
	return types;
}

export function loadAuthoritativeCommandTypes(): string[] {
	return deriveRpcCommandTypes(readFileSync(AUTHORITATIVE_RPC_TYPES_PATH, "utf8"));
}

/**
 * Every authoritative command must be exercised by the scenario, and the
 * scenario must not claim coverage of a command that no longer exists.
 */
export function assertFullCoverage(scenarioTypes: ReadonlySet<string>, authoritative: readonly string[]): void {
	const missing = authoritative.filter((type) => !scenarioTypes.has(type));
	const extra = [...scenarioTypes].filter((type) => !authoritative.includes(type));
	if (missing.length > 0) {
		fail(
			`rpc-parity scenario does not cover authoritative RPC command(s): ${missing.join(", ")}. ` +
				"A new RPC command must be added to the replay scenario; refusing to silently exclude it.",
		);
	}
	if (extra.length > 0) {
		fail(`rpc-parity scenario claims unknown RPC command(s): ${extra.join(", ")}`);
	}
}

// ============================================================================
// Scenario
// ============================================================================

export interface ScenarioState {
	workDir: string;
	sessionFile?: string;
	firstEntryId?: string;
	forkEntryId?: string;
}

export interface ScenarioStep {
	/** Unique correlation id; deterministic and identical for both binaries. */
	readonly name: string;
	/** Authoritative discriminant, or undefined for the unknown-command probe. */
	readonly commandType?: string;
	/** Wait for an agent_settled event after the response. */
	readonly settle?: boolean;
	readonly build: (state: ScenarioState) => JsonObject;
	readonly harvest?: (response: JsonObject, state: ScenarioState) => void;
}

function requireString(value: JsonValue | undefined, label: string): string {
	assert(typeof value === "string" && value.length > 0, `${label} must be a non-empty string`);
	return value;
}

function responseData(response: JsonObject, label: string): JsonObject {
	const data = response.data;
	assert(isObject(data), `${label} response data must be an object`);
	return data;
}

/**
 * Dependency-valid replay order: state-free commands run on the fresh
 * session, prompts create history, then history-dependent commands harvest
 * real ids from prior responses. Expected errors are parity outcomes, not
 * skipped commands.
 */
export function buildScenario(): ScenarioStep[] {
	let sequence = 0;
	const step = (
		commandType: string,
		fields: JsonObject,
		options: { settle?: boolean; suffix?: string; harvest?: ScenarioStep["harvest"] } = {},
	): ScenarioStep => {
		sequence += 1;
		const name = `c${String(sequence).padStart(2, "0")}-${commandType}${options.suffix ?? ""}`;
		const scenarioStep: ScenarioStep = {
			name,
			commandType,
			build: () => ({ id: name, type: commandType, ...fields }),
		};
		if (options.settle === true) return { ...scenarioStep, settle: true };
		if (options.harvest) return { ...scenarioStep, harvest: options.harvest };
		return scenarioStep;
	};

	const steps: ScenarioStep[] = [
		step("get_state", {}),
		step("get_commands", {}),
		step("get_available_models", {}),
		step("set_model", { provider: VERIFICATION_PROVIDER, modelId: VERIFICATION_MODEL }),
		step("set_thinking_level", { level: "off" }),
		step("cycle_thinking_level", {}),
		step("cycle_model", {}),
		step("set_model", { provider: VERIFICATION_PROVIDER, modelId: VERIFICATION_MODEL }, { suffix: "-restore" }),
		step("set_steering_mode", { mode: "one-at-a-time" }),
		step("set_follow_up_mode", { mode: "one-at-a-time" }),
		step("set_auto_compaction", { enabled: false }),
		step("set_auto_retry", { enabled: false }),
		step("abort_retry", {}),
		step("abort", {}),
		step("abort_bash", {}),
		step("set_session_name", { name: "rpc-parity-session" }),
		step("bash", { command: "printf 'rpc-parity-bash\\n'" }),
		step("prompt", { message: "Reply with deterministic verification text." }, { settle: true }),
		step("prompt", { message: "verification:tools exercise the tool turn." }, { settle: true, suffix: "-tools" }),
		step("steer", { message: "queued steering note" }),
		step("follow_up", { message: "queued follow-up note" }),
	];

	sequence = steps.length;
	steps.push({
		name: "c22-get_state-harvest",
		commandType: "get_state",
		build: () => ({ id: "c22-get_state-harvest", type: "get_state" }),
		harvest: (response, state) => {
			state.sessionFile = requireString(responseData(response, "get_state").sessionFile, "get_state sessionFile");
		},
	});
	steps.push({
		name: "c23-prompt-flush",
		commandType: "prompt",
		settle: true,
		build: () => ({ id: "c23-prompt-flush", type: "prompt", message: "Flush the queued messages." }),
	});
	steps.push({
		name: "c24-get_last_assistant_text",
		commandType: "get_last_assistant_text",
		build: () => ({ id: "c24-get_last_assistant_text", type: "get_last_assistant_text" }),
	});
	steps.push({
		name: "c25-get_messages",
		commandType: "get_messages",
		build: () => ({ id: "c25-get_messages", type: "get_messages" }),
	});
	steps.push({
		name: "c26-get_session_stats",
		commandType: "get_session_stats",
		build: () => ({ id: "c26-get_session_stats", type: "get_session_stats" }),
	});
	steps.push({
		name: "c27-get_entries",
		commandType: "get_entries",
		build: () => ({ id: "c27-get_entries", type: "get_entries" }),
		harvest: (response, state) => {
			const entries = responseData(response, "get_entries").entries;
			assert(Array.isArray(entries) && entries.length > 0, "get_entries must return entries");
			const first = entries[0];
			assert(isObject(first), "get_entries first entry must be an object");
			state.firstEntryId = requireString(first.id, "get_entries first entry id");
		},
	});
	steps.push({
		name: "c28-get_entries-since",
		commandType: "get_entries",
		build: (state) => ({
			id: "c28-get_entries-since",
			type: "get_entries",
			since: requireString(state.firstEntryId, "harvested first entry id"),
		}),
	});
	steps.push({
		name: "c29-get_tree",
		commandType: "get_tree",
		build: () => ({ id: "c29-get_tree", type: "get_tree" }),
	});
	steps.push({
		name: "c30-get_fork_messages",
		commandType: "get_fork_messages",
		build: () => ({ id: "c30-get_fork_messages", type: "get_fork_messages" }),
		harvest: (response, state) => {
			const messages = responseData(response, "get_fork_messages").messages;
			assert(Array.isArray(messages) && messages.length > 0, "get_fork_messages must return messages");
			const first = messages[0];
			assert(isObject(first), "get_fork_messages first message must be an object");
			state.forkEntryId = requireString(first.entryId, "get_fork_messages first entryId");
		},
	});
	steps.push({
		name: "c31-fork",
		commandType: "fork",
		build: (state) => ({
			id: "c31-fork",
			type: "fork",
			entryId: requireString(state.forkEntryId, "harvested fork entry id"),
		}),
	});
	steps.push({
		name: "c32-get_state-postfork",
		commandType: "get_state",
		build: () => ({ id: "c32-get_state-postfork", type: "get_state" }),
	});
	steps.push({
		name: "c33-clone",
		commandType: "clone",
		build: () => ({ id: "c33-clone", type: "clone" }),
	});
	steps.push({
		name: "c34-get_state-postclone",
		commandType: "get_state",
		build: () => ({ id: "c34-get_state-postclone", type: "get_state" }),
	});
	steps.push({
		name: "c35-new_session",
		commandType: "new_session",
		build: () => ({ id: "c35-new_session", type: "new_session" }),
	});
	steps.push({
		name: "c36-get_state-postnew",
		commandType: "get_state",
		build: () => ({ id: "c36-get_state-postnew", type: "get_state" }),
	});
	steps.push({
		name: "c37-switch_session",
		commandType: "switch_session",
		build: (state) => ({
			id: "c37-switch_session",
			type: "switch_session",
			sessionPath: requireString(state.sessionFile, "harvested session file"),
		}),
	});
	steps.push({
		name: "c38-compact",
		commandType: "compact",
		build: () => ({ id: "c38-compact", type: "compact" }),
	});
	steps.push({
		name: "c39-export_html",
		commandType: "export_html",
		build: (state) => ({
			id: "c39-export_html",
			type: "export_html",
			outputPath: join(state.workDir, "rpc-parity-export.html"),
		}),
	});
	steps.push({
		name: "c40-unknown-probe",
		build: () => ({ id: "c40-unknown-probe", type: "rpc_parity_probe", payload: { value: 1 } }),
	});
	steps.push({
		name: "c41-get_state-final",
		commandType: "get_state",
		build: () => ({ id: "c41-get_state-final", type: "get_state" }),
	});
	return steps;
}

export function scenarioCommandTypes(steps: readonly ScenarioStep[]): Set<string> {
	const types = new Set<string>();
	for (const scenarioStep of steps) {
		if (scenarioStep.commandType !== undefined) types.add(scenarioStep.commandType);
	}
	return types;
}

// ============================================================================
// Normalization (generated ids, timestamps, temporary paths only)
// ============================================================================

export interface NormalizeContext {
	/** Per-run temporary roots replaced with <tmp>. */
	readonly volatileRoots: readonly string[];
	readonly repoRoot: string;
}

const UUID_RE = /[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}/g;
// ISO instants in wire form (2026-07-26T13:00:00.123Z) and the filename-safe
// variant used in session file names (2026-07-26T13-00-00-123Z).
const ISO_TIMESTAMP_RE = /\d{4}-\d{2}-\d{2}T\d{2}[:\-]\d{2}[:\-]\d{2}(?:[.\-]\d{1,9})?(?:Z|[+\-]\d{2}:?\d{2})?/g;
const ID_KEYS = new Set([
	"id",
	"parentId",
	"leafId",
	"entryId",
	"firstKeptEntryId",
	"targetId",
	"fromId",
	"sessionId",
	"since",
]);
// Generated short entry ids are the first 8 hex chars of a UUID; other id-key
// values (model ids, deterministic tool-call ids, our own correlation ids)
// must stay literal so a real cross-binary difference cannot hide behind the
// mapping.
const GENERATED_SHORT_ID_RE = /^[0-9a-f]{8}$/;
const EPOCH_KEYS = new Set(["timestamp"]);
const DURATION_KEY_RE = /(?:durationMs|elapsedMs|executionTimeMs|latencyMs|timeMs)$/;

interface NormalizeState {
	readonly context: NormalizeContext;
	readonly generatedIds: Map<string, string>;
}

function mapGenerated(state: NormalizeState, kind: "uuid" | "id", raw: string): string {
	const key = `${kind}:${raw}`;
	const existing = state.generatedIds.get(key);
	if (existing !== undefined) return existing;
	const token = `<${kind}-${state.generatedIds.size + 1}>`;
	state.generatedIds.set(key, token);
	return token;
}

function normalizeString(value: string, key: string | undefined, state: NormalizeState): string {
	let result = value;
	for (const root of state.context.volatileRoots) {
		if (root.length > 0) result = result.split(root).join("<tmp>");
	}
	result = result.split(state.context.repoRoot).join("<repo>");
	result = result.replace(UUID_RE, (uuid) => mapGenerated(state, "uuid", uuid.toLowerCase()));
	result = result.replace(ISO_TIMESTAMP_RE, "<ts>");
	if (key !== undefined && ID_KEYS.has(key) && result === value && GENERATED_SHORT_ID_RE.test(value)) {
		return mapGenerated(state, "id", value);
	}
	return result;
}

function normalizeValue(value: JsonValue, key: string | undefined, state: NormalizeState): JsonValue {
	if (typeof value === "string") return normalizeString(value, key, state);
	if (typeof value === "number") {
		if (key !== undefined && (EPOCH_KEYS.has(key) || DURATION_KEY_RE.test(key))) return 0;
		return value;
	}
	if (Array.isArray(value)) return value.map((element) => normalizeValue(element, undefined, state));
	if (isObject(value)) {
		const normalized: JsonObject = {};
		// Traverse keys in sorted order so generated-id placeholders are
		// allocated deterministically regardless of insertion order; two
		// semantically equal objects with different field order must map
		// their generated ids to the same placeholders.
		for (const [childKey, childValue] of Object.entries(value).sort(([left], [right]) =>
			left < right ? -1 : Number(left > right),
		)) {
			normalized[childKey] = normalizeValue(childValue, childKey, state);
		}
		return normalized;
	}
	return value;
}

/**
 * Normalize one binary's transcript by scrubbing generated ids, timestamps, and temporary paths.
 */
export function normalizeTranscript(records: readonly JsonObject[], context: NormalizeContext): JsonValue[] {
	const state: NormalizeState = { context, generatedIds: new Map() };
	return records.map((record) => normalizeValue(record, undefined, state));
}

// ============================================================================
// Process driver
// ============================================================================

interface DriveResult {
	readonly label: string;
	readonly runRoot: string;
	readonly transcript: JsonObject[];
	readonly exitCode: number;
	readonly stderrTail: string;
}

class TranscriptWaiter {
	readonly records: JsonObject[] = [];
	private waiters: Array<{
		readonly from: number;
		readonly predicate: (record: JsonObject) => boolean;
		readonly resolve: (index: number) => void;
	}> = [];

	push(record: JsonObject): void {
		this.records.push(record);
		const index = this.records.length - 1;
		this.waiters = this.waiters.filter((waiter) => {
			if (index < waiter.from || !waiter.predicate(record)) return true;
			waiter.resolve(index);
			return false;
		});
	}

	waitFor(
		predicate: (record: JsonObject) => boolean,
		from: number,
		deadlineMs: number,
		label: string,
	): Promise<number> {
		for (let index = from; index < this.records.length; index++) {
			const record = this.records[index];
			if (record !== undefined && predicate(record)) return Promise.resolve(index);
		}
		return new Promise((resolvePromise, rejectPromise) => {
			const timer = setTimeout(() => {
				this.waiters = this.waiters.filter((waiter) => waiter.resolve !== resolveWrapped);
				rejectPromise(new Error(`timed out after ${deadlineMs}ms waiting for ${label}`));
			}, deadlineMs);
			const resolveWrapped = (index: number): void => {
				clearTimeout(timer);
				resolvePromise(index);
			};
			this.waiters.push({ from, predicate, resolve: resolveWrapped });
		});
	}
}

function processEnvironment(runRoot: string): Record<string, string> {
	const env: Record<string, string> = {
		...(process.env as Record<string, string>),
		HOME: join(runRoot, "home"),
		PI_CODING_AGENT_DIR: join(runRoot, "agent"),
		PI_CODING_AGENT_SESSION_DIR: join(runRoot, "sessions"),
		PI_EXTENSION_HOST: EXTENSION_HOST,
		PI_OFFLINE: "1",
		PI_VERIFICATION_MODE: "auto",
		PI_VERIFICATION_CHUNK_COUNT: "3",
		PI_VERIFICATION_CHUNK_DELAY_MS: "0",
		PI_VERIFICATION_LOAD_COUNT_PATH: join(runRoot, "extension-load-generation.txt"),
		PI_VERIFICATION_COMPATIBILITY_PATH: join(runRoot, "compatibility.jsonl"),
		PI_VERIFICATION_TOOL_PATH: TOOL_FILE,
	};
	return env;
}

function commonArguments(): string[] {
	return [
		"--mode",
		"rpc",
		"--provider",
		VERIFICATION_PROVIDER,
		"--model",
		VERIFICATION_MODEL,
		"--api-key",
		"verification-key",
		"--extension",
		EXTENSION_PATH,
		"--offline",
		"--no-context-files",
		"--no-skills",
		"--no-prompt-templates",
		"--no-themes",
		"--approve",
	];
}

async function driveBinary(label: string, argv: readonly string[], runRoot: string): Promise<DriveResult> {
	const workDir = join(runRoot, "work");
	for (const directory of ["home", "agent", "sessions", "work"]) {
		mkdirSync(join(runRoot, directory), { recursive: true });
	}
	writeFileSync(join(workDir, TOOL_FILE), "verification-before\n", "utf8");

	const child = Bun.spawn({
		cmd: [...argv],
		cwd: workDir,
		env: processEnvironment(runRoot),
		stdin: "pipe",
		stdout: "pipe",
		stderr: "pipe",
	});

	const transcript = new TranscriptWaiter();
	let stderrTail = "";
	const stderrPump = (async () => {
		const decoder = new TextDecoder();
		for await (const chunk of child.stderr) {
			stderrTail = (stderrTail + decoder.decode(chunk, { stream: true })).slice(-65_536);
		}
	})();
	const stdoutPump = (async () => {
		const decoder = new TextDecoder();
		let buffered = "";
		for await (const chunk of child.stdout) {
			buffered += decoder.decode(chunk, { stream: true });
			let newline = buffered.indexOf("\n");
			while (newline !== -1) {
				const line = buffered.slice(0, newline).replace(/\r$/, "").trim();
				buffered = buffered.slice(newline + 1);
				newline = buffered.indexOf("\n");
				if (line.length === 0) continue;
				let parsed: JsonValue;
				try {
					parsed = JSON.parse(line) as JsonValue;
				} catch (error) {
					fail(`${label}: non-JSON stdout line: ${line.slice(0, 500)} (${String(error)})`);
				}
				assert(isObject(parsed), `${label}: stdout line is not a JSON object`);
				transcript.push(parsed);
			}
		}
	})();

	const state: ScenarioState = { workDir };
	try {
		for (const scenarioStep of buildScenario()) {
			const command = scenarioStep.build(state);
			const from = transcript.records.length;
			child.stdin.write(`${JSON.stringify(command)}\n`);
			child.stdin.flush();
			const responseIndex = await transcript.waitFor(
				(record) => record.type === "response" && record.id === scenarioStep.name,
				from,
				STEP_DEADLINE_MS,
				`${label} response to ${scenarioStep.name}`,
			);
			if (scenarioStep.settle === true) {
				await transcript.waitFor(
					(record) => record.type === "agent_settled",
					from,
					STEP_DEADLINE_MS,
					`${label} agent_settled after ${scenarioStep.name}`,
				);
			}
			if (scenarioStep.harvest) {
				const response = transcript.records[responseIndex];
				assert(response !== undefined, `${label}: response index out of range for ${scenarioStep.name}`);
				assert(
					response.success === true,
					`${label}: ${scenarioStep.name} must succeed to harvest state, got ${JSON.stringify(response).slice(0, 500)}`,
				);
				scenarioStep.harvest(response, state);
			}
		}
	} catch (error) {
		child.kill();
		const message = error instanceof Error ? error.message : String(error);
		writeJsonl(join(runRoot, "partial-transcript.jsonl"), transcript.records);
		fail(
			`${message}\n${label} partial transcript: ${join(runRoot, "partial-transcript.jsonl")} (${transcript.records.length} records)\n${label} stderr tail:\n${stderrTail.slice(-4_000)}`,
		);
	}

	await child.stdin.end();
	const exitTimer = setTimeout(() => child.kill(), EXIT_DEADLINE_MS);
	const exitCode = await child.exited;
	clearTimeout(exitTimer);
	await Promise.all([stdoutPump, stderrPump]);
	return { label, runRoot, transcript: transcript.records, exitCode, stderrTail };
}

// ============================================================================
// Comparison and evidence
// ============================================================================

/** Key-order-insensitive serialization: JSON object key order is not wire semantics. */
export function canonicalStringify(value: JsonValue): string {
	if (Array.isArray(value)) return `[${value.map(canonicalStringify).join(",")}]`;
	if (isObject(value)) {
		const keys = Object.keys(value).sort();
		return `{${keys
			.map((key) => {
				const child = value[key];
				return `${JSON.stringify(key)}:${canonicalStringify(child ?? null)}`;
			})
			.join(",")}}`;
	}
	return JSON.stringify(value);
}

function firstDivergence(rust: readonly JsonValue[], typescript: readonly JsonValue[]): number {
	const limit = Math.min(rust.length, typescript.length);
	for (let index = 0; index < limit; index++) {
		const left = rust[index];
		const right = typescript[index];
		if (canonicalStringify(left ?? null) !== canonicalStringify(right ?? null)) return index;
	}
	return rust.length === typescript.length ? -1 : limit;
}

function renderRecord(records: readonly JsonValue[], index: number): string {
	const record = records[index];
	if (record === undefined) return "<absent>";
	const text = canonicalStringify(record);
	return text.length > 2_000 ? `${text.slice(0, 2_000)}…(${text.length} chars)` : text;
}

function writeJsonl(path: string, records: readonly JsonValue[]): void {
	writeFileSync(path, `${records.map((record) => JSON.stringify(record)).join("\n")}\n`, "utf8");
}

async function ensureRustReleaseBinary(): Promise<void> {
	if (existsSync(RUST_BINARY)) return;

	console.error(
		`rpc-parity: release binary missing; running ${RUST_RELEASE_BUILD_COMMAND.join(" ")}`,
	);
	let exitCode: number;
	try {
		exitCode = await Bun.spawn({
			cmd: RUST_RELEASE_BUILD_COMMAND,
			cwd: REPO_ROOT,
			stdout: "inherit",
			stderr: "inherit",
		}).exited;
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		fail(`rpc-parity could not start release build: ${message}`);
	}
	if (exitCode !== 0) {
		fail(`rpc-parity release build failed with exit code ${exitCode}`);
	}
	if (!existsSync(RUST_BINARY)) {
		fail(`rpc-parity release build did not create ${RUST_BINARY}`);
	}
}

async function main(): Promise<void> {
	await ensureRustReleaseBinary();
	for (const required of [
		RUST_BINARY,
		EXTENSION_HOST,
		EXTENSION_PATH,
		TYPESCRIPT_CLI,
		AUTHORITATIVE_RPC_TYPES_PATH,
	]) {
		try {
			readFileSync(required);
		} catch {
			fail(`rpc-parity prerequisite missing: ${required}`);
		}
	}

	const authoritative = loadAuthoritativeCommandTypes();
	const scenario = buildScenario();
	assertFullCoverage(scenarioCommandTypes(scenario), authoritative);

	mkdirSync(EVIDENCE_ROOT, { recursive: true });
	const runRoot = join(EVIDENCE_ROOT, `run-${Date.now()}`);
	mkdirSync(runRoot, { recursive: true });

	const bun = Bun.which("bun") ?? fail("rpc-parity prerequisite missing: bun executable");
	console.error(`rpc-parity: replaying ${authoritative.length} authoritative RPC commands over ${scenario.length} steps`);

	const rustRun = await driveBinary("rust", [RUST_BINARY, ...commonArguments()], join(runRoot, "rust"));
	console.error(`rpc-parity: rust transcript ${rustRun.transcript.length} records, exit ${rustRun.exitCode}`);
	const typescriptRun = await driveBinary(
		"typescript",
		[bun, TYPESCRIPT_CLI, ...commonArguments()],
		join(runRoot, "typescript"),
	);
	console.error(
		`rpc-parity: typescript transcript ${typescriptRun.transcript.length} records, exit ${typescriptRun.exitCode}`,
	);

	const rustNormalized = normalizeTranscript(rustRun.transcript, {
		volatileRoots: [rustRun.runRoot],
		repoRoot: REPO_ROOT,
	});
	const typescriptNormalized = normalizeTranscript(typescriptRun.transcript, {
		volatileRoots: [typescriptRun.runRoot],
		repoRoot: REPO_ROOT,
	});

	writeJsonl(join(runRoot, "rust-raw.jsonl"), rustRun.transcript);
	writeJsonl(join(runRoot, "typescript-raw.jsonl"), typescriptRun.transcript);
	writeJsonl(join(runRoot, "rust-normalized.jsonl"), rustNormalized);
	writeJsonl(join(runRoot, "typescript-normalized.jsonl"), typescriptNormalized);

	assert(
		rustRun.exitCode === 0 && typescriptRun.exitCode === 0,
		`clean-shutdown exit codes differ from 0: rust=${rustRun.exitCode} typescript=${typescriptRun.exitCode}`,
	);

	const divergence = firstDivergence(rustNormalized, typescriptNormalized);
	if (divergence !== -1) {
		const contextStart = Math.max(0, divergence - 2);
		const lines: string[] = [
			`rpc-parity FAILED: transcripts diverge at normalized record ${divergence}`,
			`rust records: ${rustNormalized.length}, typescript records: ${typescriptNormalized.length}`,
			`evidence: ${runRoot}`,
		];
		for (let index = contextStart; index <= divergence; index++) {
			lines.push(`  rust[${index}]: ${renderRecord(rustNormalized, index)}`);
			lines.push(`    ts[${index}]: ${renderRecord(typescriptNormalized, index)}`);
		}
		fail(lines.join("\n"));
	}

	const summary = {
		authoritativeCommandCount: authoritative.length,
		authoritativeCommands: authoritative,
		scenarioSteps: scenario.length,
		rustRecords: rustRun.transcript.length,
		typescriptRecords: typescriptRun.transcript.length,
		normalizedRecords: rustNormalized.length,
		equal: true,
	};
	writeFileSync(join(runRoot, "result.json"), `${JSON.stringify(summary, null, "\t")}\n`, "utf8");
	console.error(
		`rpc-parity OK: ${authoritative.length} commands, ${rustNormalized.length} normalized records identical; evidence ${runRoot}`,
	);
}

if (import.meta.main) await main();
