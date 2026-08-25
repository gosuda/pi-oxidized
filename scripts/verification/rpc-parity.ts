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
 * timestamps/elapsed-time values, and per-run temporary paths. Streaming
 * delta events (`message_update`, `tool_execution_update`) are collapsed per
 * run because delta granularity is transport chunking, not protocol content;
 * final content parity is enforced by `message_end`/`tool_execution_end`.
 */

import { mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
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
const SUPPORTED_ENV_VARS: Record<string, true> = { PI_RPC_PARITY_STEP_TIMEOUT_MS: true };
const ENV_PREFIX = "PI_RPC_PARITY_";
for (const key of Object.keys(process.env)) {
	if (key.startsWith(ENV_PREFIX) && SUPPORTED_ENV_VARS[key] !== true) {
		fail(
			`unsupported environment variable "${key}" (supported: ${Object.keys(SUPPORTED_ENV_VARS).join(", ")}); check for typos`,
		);
	}
}
const STEP_DEADLINE_MS = (() => {
	const raw = process.env.PI_RPC_PARITY_STEP_TIMEOUT_MS;
	if (raw === undefined) return 120_000;
	const value = Number(raw);
	if (!Number.isFinite(value) || !Number.isInteger(value) || value <= 0) {
		fail(
			`invalid value for PI_RPC_PARITY_STEP_TIMEOUT_MS: received "${raw}" ` +
				"(must be a positive finite integer milliseconds value)",
		);
	}
	return value;
})();
const EXIT_DEADLINE_MS = 30_000;

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
 * Build one scenario step. Both `settle` and `harvest` are spread onto the
 * returned object so a single step can carry both — never an if/return that
 * silently drops one option.
 */
export function buildScenarioStep(
	commandType: string | undefined,
	fields: JsonObject | ((state: ScenarioState) => JsonObject),
	options: { settle?: boolean; suffix?: string; harvest?: ScenarioStep["harvest"] } = {},
	sequence: number,
): ScenarioStep {
	const resolvedForName = typeof fields === "function" ? undefined : fields;
	const baseName =
		commandType ??
		(isObject(resolvedForName) && typeof resolvedForName.type === "string" ? resolvedForName.type : "unknown");
	const name = `c${String(sequence).padStart(2, "0")}-${baseName}${options.suffix ?? ""}`;
	return {
		name,
		commandType,
		build: (state: ScenarioState) => {
			const resolved = typeof fields === "function" ? fields(state) : fields;
			return { id: name, ...(commandType === undefined ? {} : { type: commandType }), ...resolved };
		},
		...(options.settle === true ? { settle: true } : {}),
		...(options.harvest ? { harvest: options.harvest } : {}),
	};
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
		commandType: string | undefined,
		fields: JsonObject | ((state: ScenarioState) => JsonObject) = {},
		options: { settle?: boolean; suffix?: string; harvest?: ScenarioStep["harvest"] } = {},
	): ScenarioStep => {
		sequence += 1;
		return buildScenarioStep(commandType, fields, options, sequence);
	};

	const steps: ScenarioStep[] = [
		step("get_state", {}),
		step("get_commands", {}),
		step("get_available_models", {}),
		step("set_model", { provider: VERIFICATION_PROVIDER, modelId: VERIFICATION_MODEL }),
		step("set_thinking_level", { level: "off" }),
		step("cycle_thinking_level", {}),
		step("get_available_thinking_levels", {}),
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

	steps.push(
		step("get_state", {}, {
			suffix: "-harvest",
			harvest: (response, state) => {
				state.sessionFile = requireString(
					responseData(response, "get_state").sessionFile,
					"get_state sessionFile",
				);
			},
		}),
	);
	steps.push(step("prompt", { message: "Flush the queued messages." }, { settle: true, suffix: "-flush" }));
	steps.push(step("get_last_assistant_text", {}));
	steps.push(step("get_messages", {}));
	steps.push(step("get_session_stats", {}));
	steps.push(
		step("get_entries", {}, {
			harvest: (response, state) => {
				const entries = responseData(response, "get_entries").entries;
				assert(Array.isArray(entries) && entries.length > 0, "get_entries must return entries");
				const first = entries[0];
				assert(isObject(first), "get_entries first entry must be an object");
				state.firstEntryId = requireString(first.id, "get_entries first entry id");
			},
		}),
	);
	steps.push(
		step("get_entries", (state) => ({ since: requireString(state.firstEntryId, "harvested first entry id") }), {
			suffix: "-since",
		}),
	);
	steps.push(step("get_tree", {}));
	steps.push(
		step("get_fork_messages", {}, {
			harvest: (response, state) => {
				const messages = responseData(response, "get_fork_messages").messages;
				assert(Array.isArray(messages) && messages.length > 0, "get_fork_messages must return messages");
				const first = messages[0];
				assert(isObject(first), "get_fork_messages first message must be an object");
				state.forkEntryId = requireString(first.entryId, "get_fork_messages first entryId");
			},
		}),
	);
	steps.push(step("fork", (state) => ({ entryId: requireString(state.forkEntryId, "harvested fork entry id") })));
	steps.push(step("get_state", {}, { suffix: "-postfork" }));
	steps.push(step("clone", {}));
	steps.push(step("new_session", {}));
	steps.push(
		step("switch_session", (state) => ({
			sessionPath: requireString(state.sessionFile, "harvested session file"),
		})),
	);
	steps.push(step("compact", {}));
	steps.push(
		step("export_html", (state) => ({
			outputPath: join(state.workDir, "rpc-parity-export.html"),
		})),
	);
	steps.push(step(undefined, { type: "rpc_parity_probe", payload: { value: 1 } }, { suffix: "-unknown-probe" }));
	steps.push(step("get_state", {}, { suffix: "-final" }));
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
const COLLAPSIBLE_EVENT_TYPES = new Set(["message_update", "tool_execution_update"]);

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
		for (const [childKey, childValue] of Object.entries(value)) {
			normalized[childKey] = normalizeValue(childValue, childKey, state);
		}
		return normalized;
	}
	return value;
}

/**
 * Normalize one binary's transcript: collapse streaming delta runs, then
 * scrub generated ids, timestamps, and temporary paths.
 */
export function normalizeTranscript(records: readonly JsonObject[], context: NormalizeContext): JsonValue[] {
	const state: NormalizeState = { context, generatedIds: new Map() };
	const normalized: JsonValue[] = [];
	let openCollapseType: string | undefined;
	for (const record of records) {
		const type = typeof record.type === "string" ? record.type : "";
		if (COLLAPSIBLE_EVENT_TYPES.has(type)) {
			if (openCollapseType === type) continue;
			openCollapseType = type;
			normalized.push({ type, collapsed: true });
			continue;
		}
		openCollapseType = undefined;
		normalized.push(normalizeValue(record, undefined, state));
	}
	return normalized;
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

export class TranscriptWaiter {
	readonly records: JsonObject[] = [];
	private waiters: Array<{
		readonly from: number;
		readonly predicate: (record: JsonObject) => boolean;
		readonly resolve: (index: number) => void;
		readonly reject: (error: Error) => void;
		readonly cancel: () => void;
	}> = [];
	private aborted: Error | null = null;

	push(record: JsonObject): void {
		this.records.push(record);
		const index = this.records.length - 1;
		this.waiters = this.waiters.filter((waiter) => {
			if (index < waiter.from || !waiter.predicate(record)) return true;
			waiter.cancel();
			waiter.resolve(index);
			return false;
		});
	}

	/** Immediately reject all pending waiters with `error` and reject future waits. */
	abort(error: Error): void {
		if (this.aborted) return;
		this.aborted = error;
		const waiters = this.waiters;
		this.waiters = [];
		for (const waiter of waiters) {
			waiter.cancel();
			waiter.reject(error);
		}
	}

	waitFor(
		predicate: (record: JsonObject) => boolean,
		from: number,
		deadlineMs: number,
		label: string,
	): Promise<number> {
		if (this.aborted) return Promise.reject(this.aborted);
		for (let index = from; index < this.records.length; index++) {
			const record = this.records[index];
			if (record !== undefined && predicate(record)) return Promise.resolve(index);
		}
		const { promise, resolve: resolveRaw, reject: rejectRaw } = Promise.withResolvers<number>();
		const timer = setTimeout(() => {
			this.waiters = this.waiters.filter((waiter) => waiter.resolve !== resolve);
			rejectRaw(new Error(`timed out after ${deadlineMs}ms waiting for ${label}`));
		}, deadlineMs);
		const cancel = (): void => clearTimeout(timer);
		const resolve = (index: number): void => {
			cancel();
			resolveRaw(index);
		};
		const reject = (error: Error): void => {
			cancel();
			rejectRaw(error);
		};
		this.waiters.push({ from, predicate, resolve, reject, cancel });
		return promise;
	}
}

/**
 * Single ownership contract shared by the stdout and stderr stream pumps: the
 * first async-iterator failure is normalized to a real Error, recorded in
 * shared state, and broadcast to every pending transcript waiter — so a stream
 * pump death surfaces as a scenario failure instead of stalling a `waitFor` or
 * leaking an unhandled rejection. The FIRST normalized Error owns
 * `holder.pumpError` and is used to abort pending or future waiters; a later
 * failure on the other stream does not replace it. `transcript.abort` is
 * idempotent, so the second call is a no-op once the first error has aborted
 * the transcript.
 */
export interface PumpErrorHolder {
	pumpError: Error | null;
}

export function recordPumpFailure(
	error: unknown,
	holder: PumpErrorHolder,
	transcript: { abort(error: Error): void },
): void {
	const normalized = error instanceof Error ? error : new Error(String(error));
	if (holder.pumpError === null) holder.pumpError = normalized;
	transcript.abort(holder.pumpError);
}

/**
 * Iterate `stream` into `onChunk`, forwarding any async-iterator failure to
 * `onFailure` instead of rejecting. Resolving (rather than rejecting) on
 * failure is what keeps a stream pump from becoming an unhandled rejection:
 * the failure is observed solely through `onFailure`, which records and aborts.
 */
export async function drainStream(
	stream: AsyncIterable<Uint8Array> | null,
	onChunk: (chunk: Uint8Array) => void,
	onFailure: (error: unknown) => void,
): Promise<void> {
	if (stream === null) return;
	try {
		for await (const chunk of stream) onChunk(chunk);
	} catch (error) {
		onFailure(error);
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
	const pumpHolder: PumpErrorHolder = { pumpError: null };
	const recordFailure = (error: unknown): void => recordPumpFailure(error, pumpHolder, transcript);
	const stderrDecoder = new TextDecoder();
	const stderrPump = drainStream(
		child.stderr,
		(chunk) => {
			stderrTail = (stderrTail + stderrDecoder.decode(chunk, { stream: true })).slice(-65_536);
		},
		recordFailure,
	);
	const stdoutPump = (async () => {
		try {
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
		} catch (error) {
			recordFailure(error);
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
	if (pumpHolder.pumpError) throw pumpHolder.pumpError;
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

async function main(): Promise<void> {
	for (const required of [RUST_BINARY, EXTENSION_HOST, EXTENSION_PATH, TYPESCRIPT_CLI, AUTHORITATIVE_RPC_TYPES_PATH]) {
		try {
			statSync(required);
		} catch {
			fail(
				`rpc-parity prerequisite missing: ${required}${
					required === RUST_BINARY ? "; run cargo build -p pi --release --locked" : ""
				}`,
			);
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
