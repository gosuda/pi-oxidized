/**
 * PERF-T5 dispatch-only tool benchmark (issue #93).
 *
 * Times per-call tool dispatch on both implementations with a no-op
 * deterministic tool, exercising exactly the units the ticket names:
 * argument validation, tool start/update/end events, result construction,
 * and session append. Real `read`/`edit`/`bash` dispatch stays out of this
 * bench on purpose — it is confirmed end to end by
 * `scripts/verification/e2e-smoke.ts`, whose filesystem/shell work varies
 * independently of dispatch.
 *
 * Implementations:
 * - Rust: `target/release/pi_tool_dispatch_bench` drives the production
 *   `pi_agent::execute_tool_calls` entry in-process.
 * - TypeScript: this file in `--worker` mode drives upstream `runAgentLoop`
 *   from `.references/pi` (pinned `8fa7eebd235355522c8104166b4f1f959b4e2f10`)
 *   with a deterministic scripted stream function. Upstream keeps
 *   `executeToolCalls` module-private, so the loop is the closest public
 *   path that executes upstream's real dispatch code.
 *
 * Matched boundary: the timed slice starts when the event sink receives
 * `tool_execution_start` and ends when the sink has appended the tool-result
 * message to a real `SessionManager` JSONL file. Loop/stream overhead sits
 * outside the slice on both implementations. Per call, before the slice, the
 * assistant message carrying the tool call is appended to the session on both
 * sides as well.
 *
 * Every sample is a fresh process (no JIT/runtime carry-over), alternated
 * rust/typescript per sample index to cancel sequential drift. Distributions
 * are noise-gated via `scripts/statistics.ts` `requireQuiet`
 * (relative spread > 20% rejects the verdict with the remediation ladder).
 *
 * Writes `target/bench/tool-dispatch.json` (gitignored via `target/`).
 *
 * Usage (from repo root):
 *   bun run scripts/bench-tool-dispatch.ts [--quick]
 * Worker mode (spawned by this script; also exercised directly by tests):
 *   bun run scripts/bench-tool-dispatch.ts --worker --calls N --warmup W \
 *     --blocks B --session-dir DIR [--arguments invalid]
 */
import { mkdirSync, mkdtempSync, rmSync, statSync, writeFileSync } from "node:fs";
import { arch, cpus, hostname, platform, release, tmpdir, totalmem } from "node:os";
import { join, resolve } from "node:path";
import {
	EventStream,
	type AssistantMessage,
	type AssistantMessageEvent,
	type Message,
	type Model,
	type UserMessage,
} from "../.references/pi/packages/ai/dist/index.js";
import {
	runAgentLoop,
	type AgentContext,
	type AgentEvent,
	type AgentLoopConfig,
	type AgentMessage,
	type AgentTool,
	type AgentToolResult,
} from "../.references/pi/packages/agent/dist/index.js";
import { SessionManager } from "../.references/pi/packages/coding-agent/dist/core/session-manager.js";
import {
	NOISE_EXIT_CODE,
	NOISE_RELATIVE_SPREAD_LIMIT,
	NoiseRejection,
	REMEDIATION_LADDER,
	formatNoiseRejection,
	requireQuiet,
	spreadStats,
	type NoisyDistribution,
} from "./statistics.ts";

// ── Shared protocol constants ─────────────────────────────────────────────

export type Implementation = "rust" | "typescript";
export const IMPLEMENTATIONS: readonly Implementation[] = ["rust", "typescript"];

export const RUST_BIN = resolve(import.meta.dirname, "../target/release/pi_tool_dispatch_bench");
export const UPSTREAM_PIN = "8fa7eebd235355522c8104166b4f1f959b4e2f10";

/** Argument payloads are byte-identical on both implementations. */
export const VALID_ARGUMENTS: Record<string, unknown> = {
	path: "bench/noop/input.txt",
	count: 3,
};
export const INVALID_ARGUMENTS: Record<string, unknown> = {
	// count 999 exceeds the schema maximum on both implementations.
	// (Upstream TypeBox `Value.Convert` coerces a mistyped `path` like 42 to
	// "42" instead of rejecting it, so a wrong-type payload cannot be the
	// shared rejection case; Rust does not coerce.)
	path: "bench/noop/input.txt",
	count: 999,
};

/** JSON Schema for the noop tool, identical on both implementations. */
export const NOOP_PARAMETERS = {
	type: "object",
	properties: {
		path: { type: "string", minLength: 1 },
		count: { type: "integer", minimum: 1, maximum: 64 },
	},
	required: ["path"],
	additionalProperties: false,
} as const;

export interface WorkerReportBlock {
	index: number;
	calls: number;
	wallMsPerCall: number;
	wallMedianNs: number;
	wallMinNs: number;
	wallMaxNs: number;
	cpuMsPerCall: number | null;
}

export interface WorkerReport {
	implementation: Implementation;
	argumentsMode: "valid" | "invalid";
	warmupCalls: number;
	callsPerBlock: number;
	blocks: WorkerReportBlock[];
	events: { start: number; update: number; end: number; errorResults: number };
	appends: number;
	session: { file: string | null; bytesDelta: number; headerEntries: number };
	ok: boolean;
	failure: string | null;
}

export interface WorkerFlags {
	worker: boolean;
	calls: number;
	warmup: number;
	blocks: number;
	sessionDir: string;
	invalid: boolean;
}

export function parseWorkerFlags(argv: readonly string[]): WorkerFlags {
	const flags: WorkerFlags = {
		worker: false,
		calls: 3_000,
		warmup: 300,
		blocks: 1,
		sessionDir: join(tmpdir(), "pi-tool-dispatch-bench"),
		invalid: false,
	};
	const raw = [...argv];
	while (raw.length > 0) {
		const flag = raw.shift();
		if (flag === undefined) break;
		const value = (): string => {
			const next = raw.shift();
			if (next === undefined) throw new Error(`missing value for ${flag}`);
			return next;
		};
		switch (flag) {
			case "--worker":
				flags.worker = true;
				break;
			case "--calls":
				flags.calls = Number.parseInt(value(), 10);
				break;
			case "--warmup":
				flags.warmup = Number.parseInt(value(), 10);
				break;
			case "--blocks":
				flags.blocks = Number.parseInt(value(), 10);
				break;
			case "--session-dir":
				flags.sessionDir = value();
				break;
			case "--arguments": {
				const mode = value();
				if (mode !== "valid" && mode !== "invalid") {
					throw new Error(`--arguments must be valid|invalid, got ${mode}`);
				}
				flags.invalid = mode === "invalid";
				break;
			}
			default:
				throw new Error(`unknown flag ${flag}`);
		}
	}
	if (!Number.isInteger(flags.calls) || flags.calls <= 0) throw new Error("--calls must be a positive integer");
	if (!Number.isInteger(flags.warmup) || flags.warmup < 0) throw new Error("--warmup must be a non-negative integer");
	if (!Number.isInteger(flags.blocks) || flags.blocks <= 0) throw new Error("--blocks must be a positive integer");
	return flags;
}

/** Expected event/appends contract for `calls` measured calls in `mode`. */
export function protocolExpectations(mode: "valid" | "invalid", calls: number) {
	return {
		start: calls,
		update: mode === "valid" ? calls : 0,
		end: calls,
		errorResults: mode === "valid" ? 0 : calls,
		appends: calls * 2,
	};
}

/**
 * Validates a worker report against the shared dispatch protocol.
 * Returns `null` when the report is contract-clean, else a failure string.
 */
export function validateWorkerReport(
	report: WorkerReport,
	expected: { implementation: Implementation; mode: "valid" | "invalid"; calls: number },
): string | null {
	if (report.ok !== true) return `worker reported failure: ${String(report.failure)}`;
	if (report.implementation !== expected.implementation) {
		return `implementation mismatch: expected ${expected.implementation}, got ${String(report.implementation)}`;
	}
	if (report.argumentsMode !== expected.mode) {
		return `arguments mode mismatch: expected ${expected.mode}, got ${String(report.argumentsMode)}`;
	}
	if (report.blocks.length === 0) return "no measured blocks";
	const want = protocolExpectations(expected.mode, expected.calls);
	if (report.events.start !== want.start) return `start events ${report.events.start} != ${want.start}`;
	if (report.events.update !== want.update) return `update events ${report.events.update} != ${want.update}`;
	if (report.events.end !== want.end) return `end events ${report.events.end} != ${want.end}`;
	if (report.events.errorResults !== want.errorResults) {
		return `error results ${report.events.errorResults} != ${want.errorResults}`;
	}
	if (report.appends !== want.appends) return `appends ${report.appends} != ${want.appends}`;
	for (const block of report.blocks) {
		if (block.calls !== expected.calls) return `block ${block.index} calls ${block.calls} != ${expected.calls}`;
		if (!Number.isFinite(block.wallMsPerCall) || block.wallMsPerCall <= 0) {
			return `block ${block.index} wallMsPerCall must be finite and positive`;
		}
		if (block.cpuMsPerCall !== null && (!Number.isFinite(block.cpuMsPerCall) || block.cpuMsPerCall < 0)) {
			return `block ${block.index} cpuMsPerCall must be finite non-negative or null`;
		}
	}
	return null;
}

// ── TypeScript implementation worker (upstream path) ──────────────────────

class MockAssistantStream extends EventStream<AssistantMessageEvent, AssistantMessage> {
	constructor() {
		super(
			(event) => event.type === "done" || event.type === "error",
			(event) => {
				if (event.type === "done") return event.message;
				if (event.type === "error") return event.error;
				throw new Error("Unexpected event type");
			},
		);
	}
}

function createUsage() {
	return {
		input: 0,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 0,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
	};
}

function createModel(): Model<"openai-responses"> {
	return {
		id: "noop-bench",
		name: "noop-bench",
		api: "openai-responses",
		provider: "openai",
		baseUrl: "https://example.invalid",
		reasoning: false,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 8192,
		maxTokens: 1024,
	};
}

function toolCallAssistant(callId: string, args: Record<string, unknown>): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "toolCall", id: callId, name: "noop", arguments: structuredClone(args) }],
		api: "openai-responses",
		provider: "openai",
		model: "noop-bench",
		usage: createUsage(),
		stopReason: "toolUse",
		timestamp: Date.now(),
	};
}

function terminalAssistant(): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "text", text: "noop bench done" }],
		api: "openai-responses",
		provider: "openai",
		model: "noop-bench",
		usage: createUsage(),
		stopReason: "stop",
		timestamp: Date.now(),
	};
}

function userPrompt(): UserMessage {
	return { role: "user", content: "run noop", timestamp: Date.now() };
}

/** No-op deterministic tool mirroring the Rust bench tool 1:1. */
const noopTool: AgentTool<any> = {
	name: "noop",
	label: "noop",
	description: "Benchmark no-op tool; validates arguments, emits one update, returns a fixed result.",
	parameters: structuredClone(NOOP_PARAMETERS),
	execute: async (_toolCallId, params, _signal, onUpdate) => {
		const input = params as { path: string; count?: number };
		const count = input.count ?? 1;
		if (onUpdate) {
			onUpdate({
				content: [{ type: "text", text: "noop progress" }],
				details: { kind: "noop-progress", count },
			} satisfies AgentToolResult);
		}
		return {
			content: [{ type: "text", text: `noop ok: ${input.path} x${count}` }],
			details: { kind: "noop", path: input.path, count },
		} satisfies AgentToolResult;
	},
};

interface SinkState {
	session: SessionManager;
	t0: number | null;
	slices: number[];
	starts: number;
	updates: number;
	ends: number;
	errorResults: number;
	appends: number;
}

let state: SinkState;

function makeSink(session: SessionManager): (event: AgentEvent) => void {
	return (event: AgentEvent): void => {
		switch (event.type) {
			case "tool_execution_start":
				state.t0 = performance.now();
				state.starts += 1;
				break;
			case "tool_execution_update":
				state.updates += 1;
				break;
			case "tool_execution_end":
				state.ends += 1;
				if (event.isError) state.errorResults += 1;
				break;
			case "message_end": {
				const message = event.message;
				if (message.role === "assistant" && message.content.some((c) => c.type === "toolCall")) {
					// Pre-slice append of the trigger message, mirroring the Rust bench.
					state.session.appendMessage(message);
					state.appends += 1;
				} else if (message.role === "toolResult") {
					state.session.appendMessage(message);
					state.appends += 1;
					if (state.t0 !== null) {
						state.slices.push(performance.now() - state.t0);
						state.t0 = null;
					}
				}
				break;
			}
			default:
				break;
		}
	};
}

async function runWorkerBlock(
	sink: (event: AgentEvent) => void,
	context: AgentContext,
	config: AgentLoopConfig,
	args: Record<string, unknown>,
	calls: number,
	streamFn: () => MockAssistantStream,
	blockIndex: number,
): Promise<WorkerReportBlock> {
	state.slices = [];
	const cpuBefore = process.cpuUsage();
	for (let i = 0; i < calls; i++) {
		nextCallId = `call-${i}`;
		nextArgs = structuredClone(args);
		await runAgentLoop([userPrompt()], context, config, sink, undefined, streamFn);
	}
	const cpuDelta = process.cpuUsage(cpuBefore);
	const slicesMs = state.slices;
	if (slicesMs.length !== calls) {
		throw new Error(`expected ${calls} timed slices, got ${slicesMs.length}`);
	}
	const sorted = [...slicesMs].sort((a, b) => a - b);
	const totalMs = slicesMs.reduce((sum, value) => sum + value, 0);
	return {
		index: blockIndex,
		calls,
		wallMsPerCall: totalMs / calls,
		wallMedianNs: Math.round(sorted[Math.floor(calls / 2)] * 1_000_000),
		wallMinNs: Math.round(sorted[0] * 1_000_000),
		wallMaxNs: Math.round(sorted[calls - 1] * 1_000_000),
		cpuMsPerCall: (cpuDelta.user + cpuDelta.system) / 1000 / calls,
	};
}

// Deterministic stream script: odd stream call returns the tool-call message,
// even stream call returns the terminal message (2 turns per runAgentLoop).
let nextCallId = "call-0";
let nextArgs: Record<string, unknown> = structuredClone(VALID_ARGUMENTS);

function makeStreamFn(): () => MockAssistantStream {
	let streamCalls = 0;
	return () => {
		const stream = new MockAssistantStream();
		const callId = nextCallId;
		const args = nextArgs;
		const turn = streamCalls++;
		queueMicrotask(() => {
			if (turn % 2 === 0) {
				stream.push({ type: "done", reason: "toolUse", message: toolCallAssistant(callId, args) });
			} else {
				stream.push({ type: "done", reason: "stop", message: terminalAssistant() });
			}
		});
		return stream;
	};
}

async function runWorker(flags: WorkerFlags): Promise<number> {
	mkdirSync(flags.sessionDir, { recursive: true });
	const session = SessionManager.create(process.cwd(), flags.sessionDir);
	state = {
		session,
		t0: null,
		slices: [],
		starts: 0,
		updates: 0,
		ends: 0,
		errorResults: 0,
		appends: 0,
	};
	const sink = makeSink(session);
	const context: AgentContext = { systemPrompt: "", messages: [], tools: [noopTool] };
	const convertToLlm = (messages: AgentMessage[]): Message[] =>
		messages.filter((m) => m.role === "user" || m.role === "assistant" || m.role === "toolResult") as Message[];
	const config: AgentLoopConfig = { model: createModel(), convertToLlm };
	const streamFn = makeStreamFn();
	const args = flags.invalid ? INVALID_ARGUMENTS : VALID_ARGUMENTS;

	// Warmup on the same path; counters snapshot after it.
	nextArgs = structuredClone(args);
	await runWorkerBlock(sink, context, config, args, flags.warmup, streamFn, 0);
	const warmup = {
		starts: state.starts,
		updates: state.updates,
		ends: state.ends,
		errorResults: state.errorResults,
		appends: state.appends,
	};
	const fileAfterWarmup = session.getSessionFile();
	const bytesAfterWarmup = fileAfterWarmup ? statSync(fileAfterWarmup).size : 0;

	const blocks: WorkerReportBlock[] = [];
	for (let blockIndex = 0; blockIndex < flags.blocks; blockIndex++) {
		nextArgs = structuredClone(args);
		blocks.push(await runWorkerBlock(sink, context, config, args, flags.calls, streamFn, blockIndex));
	}

	const file = session.getSessionFile();
	const bytesAfter = file ? statSync(file).size : 0;
	const report: WorkerReport = {
		implementation: "typescript",
		argumentsMode: flags.invalid ? "invalid" : "valid",
		warmupCalls: flags.warmup,
		callsPerBlock: flags.calls,
		blocks,
		events: {
			start: state.starts - warmup.starts,
			update: state.updates - warmup.updates,
			end: state.ends - warmup.ends,
			errorResults: state.errorResults - warmup.errorResults,
		},
		appends: state.appends - warmup.appends,
		session: {
			file: file ?? null,
			bytesDelta: bytesAfter - bytesAfterWarmup,
			headerEntries: 1,
		},
		ok: true,
		failure: null,
	};

	const failure = validateWorkerReport(report, {
		implementation: "typescript",
		mode: flags.invalid ? "invalid" : "valid",
		calls: flags.calls * flags.blocks,
	});
	if (failure !== null) {
		report.ok = false;
		report.failure = failure;
	}
	process.stdout.write(`${JSON.stringify(report)}\n`);
	return report.ok ? 0 : 1;
}

// ── Orchestrator ──────────────────────────────────────────────────────────

export interface Distribution {
	count: number;
	median: number;
	p95: number;
	p99: number;
	min: number;
	max: number;
	stddev: number;
	relativeSpread: number | null;
}

export function distribution(values: readonly number[]): Distribution {
	if (values.length === 0 || values.some((value) => !Number.isFinite(value) || value < 0)) {
		throw new Error("distribution requires finite non-negative samples");
	}
	const sorted = [...values].sort((left, right) => left - right);
	const quantile = (probability: number): number => {
		if (sorted.length === 1) return sorted[0] ?? 0;
		const position = (sorted.length - 1) * probability;
		const lower = Math.floor(position);
		const upper = Math.ceil(position);
		const lowerValue = sorted[lower];
		const upperValue = sorted[upper];
		if (lowerValue === undefined || upperValue === undefined) {
			throw new Error("quantile index escaped sample range");
		}
		return lowerValue + (upperValue - lowerValue) * (position - lower);
	};
	const median = quantile(0.5);
	const spread = spreadStats(values, median);
	return {
		count: sorted.length,
		median,
		p95: quantile(0.95),
		p99: quantile(0.99),
		min: sorted[0] ?? 0,
		max: sorted.at(-1) ?? 0,
		stddev: spread.stddev,
		relativeSpread: spread.relativeSpread,
	};
}

export function implementationOrder(index: number): readonly Implementation[] {
	return index % 2 === 0 ? IMPLEMENTATIONS : ["typescript", "rust"];
}

/** Kernel clock-tick rate for /proc stat CPU counters (Rust worker). */
function clockTicksPerSecond(): number {
	const spawned = Bun.spawnSync(["getconf", "CLK_TCK"], { stdout: "pipe", stderr: "pipe" });
	const parsed = Number.parseInt(new TextDecoder().decode(spawned.stdout).trim(), 10);
	return Number.isInteger(parsed) && parsed > 0 ? parsed : 100;
}

function machineMetadata() {
	const cpuList = cpus();
	const first = cpuList[0];
	return {
		hostname: hostname(),
		platform: platform(),
		arch: arch(),
		kernel: release(),
		cpuModel: first?.model ?? "unknown",
		cpuCount: cpuList.length,
		totalMemBytes: totalmem(),
		bunVersion: Bun.version,
		nodePlatform: process.platform,
		date: new Date().toISOString(),
	};
}

interface SampleResult {
	wallMsPerCall: number;
	cpuMsPerCall: number | null;
}

function spawnWorkerSample(
	implementation: Implementation,
	calls: number,
	warmup: number,
	sessionDir: string,
): WorkerReport {
	const flags = [
		...(implementation === "rust" ? ["--clk-tck", String(clockTicksPerSecond())] : []),
		"--calls",
		String(calls),
		"--warmup",
		String(warmup),
		"--blocks",
		"1",
		"--session-dir",
		sessionDir,
		"--arguments",
		"valid",
	];
	const spawned =
		implementation === "rust"
			? Bun.spawnSync([RUST_BIN, ...flags], { stdout: "pipe", stderr: "pipe" })
			: Bun.spawnSync([process.execPath, import.meta.path, "--worker", ...flags], {
					stdout: "pipe",
					stderr: "pipe",
				});
	if (spawned.exitCode !== 0) {
		throw new Error(
			`${implementation} worker exited with ${spawned.exitCode}: ${new TextDecoder()
				.decode(spawned.stderr)
				.slice(0, 2000)}`,
		);
	}
	const stdout = new TextDecoder().decode(spawned.stdout).trim();
	const report = JSON.parse(stdout) as WorkerReport;
	if (report.implementation !== implementation) {
		throw new Error(`worker reported implementation ${String(report.implementation)}, expected ${implementation}`);
	}
	return report;
}

function runSample(
	implementation: Implementation,
	sampleIndex: number,
	calls: number,
	warmup: number,
): SampleResult {
	const sessionDir = mkdtempSync(join(tmpdir(), `t5-${implementation}-${sampleIndex}-`));
	try {
		const report = spawnWorkerSample(implementation, calls, warmup, sessionDir);
		const failure = validateWorkerReport(report, { implementation, mode: "valid", calls });
		if (failure !== null) throw new Error(`${implementation} sample ${sampleIndex}: ${failure}`);
		const block = report.blocks[0];
		if (block === undefined) throw new Error(`${implementation} sample ${sampleIndex}: no blocks`);
		return { wallMsPerCall: block.wallMsPerCall, cpuMsPerCall: block.cpuMsPerCall };
	} finally {
		rmSync(sessionDir, { recursive: true, force: true });
	}
}

function buildRustBinary(): void {
	const spawned = Bun.spawnSync(
		["cargo", "build", "--release", "-p", "pi", "--bin", "pi_tool_dispatch_bench"],
		{ stdout: "inherit", stderr: "inherit" },
	);
	if (spawned.exitCode !== 0) {
		throw new Error(`cargo build exited with ${spawned.exitCode}`);
	}
}

function noiseRows(
	wall: Record<Implementation, Distribution>,
	cpu: Record<Implementation, Distribution | null>,
): NoisyDistribution[] {
	const rows: NoisyDistribution[] = [
		{
			label: "rust wall ms/call",
			count: wall.rust.count,
			median: wall.rust.median,
			stddev: wall.rust.stddev,
			relativeSpread: wall.rust.relativeSpread,
		},
		{
			label: "typescript wall ms/call",
			count: wall.typescript.count,
			median: wall.typescript.median,
			stddev: wall.typescript.stddev,
			relativeSpread: wall.typescript.relativeSpread,
		},
	];
	for (const implementation of IMPLEMENTATIONS) {
		const dist = cpu[implementation];
		if (dist !== null) {
			rows.push({
				label: `${implementation} cpu ms/call`,
				count: dist.count,
				median: dist.median,
				stddev: dist.stddev,
				relativeSpread: dist.relativeSpread,
			});
		}
	}
	return rows;
}

async function main(): Promise<number> {
	const quick = process.argv.includes("--quick");
	const samples = quick ? 3 : 10;
	const calls = quick ? 1_000 : 10_000;
	const warmup = quick ? 100 : 1_000;

	buildRustBinary();

	const wallSamples: Record<Implementation, number[]> = { rust: [], typescript: [] };
	const cpuSamples: Record<Implementation, number[]> = { rust: [], typescript: [] };
	for (let sample = 0; sample < samples; sample++) {
		for (const implementation of implementationOrder(sample)) {
			const result = runSample(implementation, sample, calls, warmup);
			wallSamples[implementation].push(result.wallMsPerCall);
			if (result.cpuMsPerCall !== null) cpuSamples[implementation].push(result.cpuMsPerCall);
			process.stderr.write(
				`tool-dispatch: sample ${sample + 1}/${samples} ${implementation} wall=${result.wallMsPerCall.toFixed(6)}ms/call cpu=${result.cpuMsPerCall === null ? "n/a" : `${result.cpuMsPerCall.toFixed(6)}ms/call`}\n`,
			);
		}
	}

	const wall: Record<Implementation, Distribution> = {
		rust: distribution(wallSamples.rust),
		typescript: distribution(wallSamples.typescript),
	};
	const cpu: Record<Implementation, Distribution | null> = {
		rust: cpuSamples.rust.length > 0 ? distribution(cpuSamples.rust) : null,
		typescript: cpuSamples.typescript.length > 0 ? distribution(cpuSamples.typescript) : null,
	};
	const speedup = {
		wall: wall.typescript.median / wall.rust.median,
		cpu:
			cpu.rust !== null && cpu.typescript !== null && cpu.rust.median > 0
				? cpu.typescript.median / cpu.rust.median
				: null,
	};
	const failures: string[] = [];
	if (wall.rust.median <= 0) failures.push("rust wall median must be positive");

	const base = {
		name: "tool-dispatch",
		lane: "PERF-T5 tool dispatch (paired comparative)",
		generatedAt: new Date().toISOString(),
		machine: machineMetadata(),
		parameters: {
			samples,
			callsPerSample: calls,
			warmupCalls: warmup,
			blocks: 1,
			noiseLimit: NOISE_RELATIVE_SPREAD_LIMIT,
			upstream: `.references/pi@${UPSTREAM_PIN}`,
			rustEntry: "pi_agent::execute_tool_calls",
			typescriptEntry: "runAgentLoop (upstream agent-loop, executeToolCalls is module-private)",
			boundary:
				"tool_execution_start event through tool-result message session append",
			protocol:
				"per call: argument validation, start/update/end events, result construction, assistant + tool-result session appends",
			tool: "noop",
		},
		results: { rust: { wall: wall.rust, cpu: cpu.rust }, typescript: { wall: wall.typescript, cpu: cpu.typescript } },
		speedup,
	};

	const outDir = resolve(process.cwd(), "target", "bench");
	mkdirSync(outDir, { recursive: true });
	const outPath = resolve(outDir, "tool-dispatch.json");

	try {
		requireQuiet(noiseRows(wall, cpu));
	} catch (error) {
		if (!(error instanceof NoiseRejection)) throw error;
		const artifact = { ...base, pass: false, failures, noise: { rejections: error.noisy, remediation: REMEDIATION_LADDER } };
		writeFileSync(outPath, `${JSON.stringify(artifact, null, 2)}\n`);
		process.stderr.write(`NOISE:\n${formatNoiseRejection(error.noisy)}\n`);
		return NOISE_EXIT_CODE;
	}

	const artifact = { ...base, pass: failures.length === 0, failures };
	writeFileSync(outPath, `${JSON.stringify(artifact, null, 2)}\n`);

	process.stderr.write(
		`tool-dispatch: pass=${artifact.pass} artifact=${outPath}\n` +
			`  rust wall median=${wall.rust.median.toFixed(6)}ms/call (spread ${((wall.rust.relativeSpread ?? 1) * 100).toFixed(2)}%)\n` +
			`  typescript wall median=${wall.typescript.median.toFixed(6)}ms/call (spread ${((wall.typescript.relativeSpread ?? 1) * 100).toFixed(2)}%)\n` +
			`  speedup TS/Rust wall=${speedup.wall.toFixed(2)}x cpu=${speedup.cpu === null ? "n/a" : `${speedup.cpu.toFixed(2)}x`}\n`,
	);
	return failures.length === 0 ? 0 : 1;
}

if (import.meta.main) {
	try {
		if (process.argv.slice(2).includes("--worker")) {
			process.exit(await runWorker(parseWorkerFlags(process.argv.slice(2))));
		}
		process.exit(await main());
	} catch (error) {
		process.stderr.write(`bench-tool-dispatch: ${error instanceof Error ? error.message : String(error)}\n`);
		process.exit(1);
	}
}
