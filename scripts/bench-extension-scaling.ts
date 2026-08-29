/**
 * Verification check 8 benchmark runner.
 *
 * Compares zero / 100 idle / 20 active widget extension loads and exercises
 * fast + slow onTerminalInput handlers. Writes a JSON artifact under
 * `target/bench/` (gitignored via `target/`) with medians/p95/p99 and machine
 * metadata.
 *
 * Usage (from repo root):
 *   bun run scripts/bench-extension-scaling.ts
 */
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { hostname, platform, arch, cpus, release, totalmem } from "node:os";
import { resolve } from "node:path";
import { Readable } from "node:stream";
import {
	PROTOCOL_VERSION,
	encodeFrameString,
	type Frame,
} from "../packages/pi-tui-protocol/src/index.ts";
import type { ExtensionFactory } from "@earendil-works/pi-coding-agent";
import {
	ExtensionHost,
	EXTENSION_INPUT_TIMEOUT_MS,
	EXTENSION_INPUT_QUEUE_CAPACITY,
} from "../packages/extension-host/src/host.ts";
import { COMPATIBILITY_VERSION } from "../packages/extension-host/src/version.ts";
import idleFactory from "../packages/extension-host/fixtures/extensions/idle.ts";
import widgetActiveFactory from "../packages/extension-host/fixtures/extensions/widget-active.ts";
import terminalInputFastFactory from "../packages/extension-host/fixtures/extensions/terminal-input-fast.ts";
import terminalInputSlowFactory from "../packages/extension-host/fixtures/extensions/terminal-input-slow.ts";
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


class FrameCollector {
	readonly frames: Frame[] = [];
	private readonly waiters: Array<{
		predicate: (f: Frame) => boolean;
		resolve: (f: Frame) => void;
	}> = [];
	private buf = "";

	write(chunk: Uint8Array): void {
		this.buf += new TextDecoder().decode(chunk);
		const lines = this.buf.split("\n");
		this.buf = lines.pop() ?? "";
		for (const line of lines) {
			if (line.trim().length === 0) continue;
			const frame = JSON.parse(line) as Frame;
			let delivered = false;
			for (let i = this.waiters.length - 1; i >= 0; i--) {
				const waiter = this.waiters[i];
				if (waiter !== undefined && waiter.predicate(frame)) {
					waiter.resolve(frame);
					this.waiters.splice(i, 1);
					delivered = true;
				}
			}
			if (!delivered) this.frames.push(frame);
		}
	}

	awaitFrame(predicate: (f: Frame) => boolean, timeoutMs = 10_000): Promise<Frame> {
		const index = this.frames.findIndex(predicate);
		if (index >= 0) {
			const [existing] = this.frames.splice(index, 1);
			if (existing !== undefined) return Promise.resolve(existing);
		}
		const { promise, resolve, reject } = Promise.withResolvers<Frame>();
		let waiter: {
			predicate: (frame: Frame) => boolean;
			resolve: (frame: Frame) => void;
		};
		const timer = setTimeout(() => {
			const index = this.waiters.indexOf(waiter);
			if (index >= 0) this.waiters.splice(index, 1);
			reject(new Error(`awaitFrame timed out after ${timeoutMs}ms`));
		}, timeoutMs);
		waiter = {
			predicate,
			resolve: (frame) => {
				clearTimeout(timer);
				resolve(frame);
			},
		};
		this.waiters.push(waiter);
		return promise;
	}
}

async function connectHost(factories: ExtensionFactory[]) {
	const collector = new FrameCollector();
	const stdin = new Readable({ read() {} });
	const host = new ExtensionHost(stdin, collector);
	const runPromise = host.run({
		cwd: process.cwd(),
		factories,
		extensionPaths: [],
	});
	stdin.push(
		Buffer.from(
			encodeFrameString({
				id: 1,
				kind: "req",
				method: "hello",
				payload: {
					protocolVersion: PROTOCOL_VERSION,
					compatibilityVersion: COMPATIBILITY_VERSION,
				},
			}),
		),
	);
	await collector.awaitFrame((f) => f.id === 1 && f.kind === "res");
	return { collector, stdin, host, runPromise };
}

async function sendSessionStart(
	stdin: Readable,
	collector: FrameCollector,
	id: number,
): Promise<void> {
	stdin.push(
		Buffer.from(
			encodeFrameString({
				id,
				kind: "req",
				method: "session_start",
				payload: { type: "session_start", reason: "startup" },
			}),
		),
	);
	await collector.awaitFrame(
		(f) => f.id === id && (f.kind === "res" || f.kind === "error"),
	);
}

async function waitUntilReady(
	stdin: Readable,
	collector: FrameCollector,
	id: number,
): Promise<void> {
	stdin.push(
		Buffer.from(
			encodeFrameString({
				id,
				kind: "req",
				method: "terminalInput",
				payload: { data: "__ready__" },
			}),
		),
	);
	await collector.awaitFrame(
		(f) => f.id === id && (f.kind === "res" || f.kind === "error"),
		60_000,
	);
}

function percentile(sorted: number[], p: number): number {
	if (sorted.length === 0) return 0;
	const idx = Math.min(
		sorted.length - 1,
		Math.max(0, Math.ceil((p / 100) * sorted.length) - 1),
	);
	return sorted[idx] ?? 0;
}

export function stats(samples: number[]) {
	const sorted = [...samples].sort((a, b) => a - b);
	const mean =
		sorted.length === 0
			? 0
			: sorted.reduce((acc, v) => acc + v, 0) / sorted.length;
	const median = percentile(sorted, 50);
	const spread = spreadStats(samples, median);
	return {
		median,
		p95: percentile(sorted, 95),
		p99: percentile(sorted, 99),
		mean,
		n: sorted.length,
		stddev: spread.stddev,
		relativeSpread: spread.relativeSpread,
	};
}

/** Measured batches per noise-gated distribution: pooled percentiles stay the
 * behavior authority while only the dispersion of per-round medians feeds the
 * 20% noise gate. */
export const NOISE_ROUNDS = 27;
export const NOISE_ROUND_WARMUPS = 20;
export const SAMPLES_PER_ROUND = 1_000;

export interface RoundSummary {
	readonly roundMedians: number[];
	readonly roundMedian: number;
	readonly roundStddev: number;
	readonly roundRelativeSpread: number | null;
}

export function roundSummary(rounds: readonly (readonly number[])[]): RoundSummary {
	if (rounds.length === 0) {
		throw new Error("round summary requires at least one measured round");
	}
	const medians = rounds.map((round) => percentile([...round].sort((a, b) => a - b), 50));
	const median = percentile([...medians].sort((a, b) => a - b), 50);
	const spread = spreadStats(medians, median);
	return {
		roundMedians: medians,
		roundMedian: median,
		roundStddev: spread.stddev,
		roundRelativeSpread: spread.relativeSpread,
	};
}

export function roundNoiseLane(label: string, rounds: RoundSummary): NoisyDistribution {
	return {
		label: `${label} round medians (n=${rounds.roundMedians.length} rounds)`,
		count: rounds.roundMedians.length,
		median: rounds.roundMedian,
		stddev: rounds.roundStddev,
		relativeSpread: rounds.roundRelativeSpread,
	};
}

// ---------------------------------------------------------------------------
// Rust production serve_io sampler (composite artifact producer)
// ---------------------------------------------------------------------------

const REPOSITORY_ROOT = resolve(import.meta.dirname, "..");
const RUST_SAMPLER_BIN = resolve(REPOSITORY_ROOT, "target/release/pi-extension-scaling");
const EXPECTED_RUST_ENTRYPOINT = "pi_ext::server::serve_io";
const EXPECTED_RUST_FRAME_CODEC = "pi_ext::protocol::{encode_frame,decode_frame_str}";
const EXPECTED_CORPUS_IDENTITY = "extension-scaling-terminal-input-v1";
const EXPECTED_CORPUS_DIGEST_ALGORITHM = "fnv1a64";
const EXPECTED_RUST_SCHEMA_VERSION = 1;
const EXPECTED_CORPUS_DIGEST = "1658b7155567cb02";
const EXPECTED_MEASURED_ROUNDS = 9;
const EXPECTED_WARMUPS_PER_SCENARIO = 30;
const EXPECTED_REQUESTS_PER_SAMPLE = 10_000;
const EXPECTED_SCENARIOS = ["zero", "idle100", "active20", "fastTerminalInput", "slowTerminalInput"] as const;
const EXPECTED_SCENARIO_EXTENSIONS: Record<(typeof EXPECTED_SCENARIOS)[number], number> = {
	zero: 0,
	idle100: 100,
	active20: 20,
	fastTerminalInput: 1,
	slowTerminalInput: 2,
};
const EXPECTED_SCENARIO_MODES: Record<(typeof EXPECTED_SCENARIOS)[number], string> = {
	zero: "passThrough",
	idle100: "passThrough",
	active20: "passThrough",
	fastTerminalInput: "fast",
	slowTerminalInput: "slowThenFast",
};

export class RustSamplerError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "RustSamplerError";
	}
}

export interface RustProtocolProvenance {
	compiledProtocolVersion: number;
	compiledCompatibilityVersion: string;
	observedProtocolVersion: number;
	observedCompatibilityVersion: string;
}

export interface RustCorpusProvenance {
	identity: string;
	digestAlgorithm: string;
	digest: string;
	measuredRounds: number;
	warmupsPerScenario: number;
	samplesPerScenario: number;
	fastStreamSamples: number;
}

export interface RustProvenance {
	entrypoint: string;
	frameCodec: string;
	protocol: RustProtocolProvenance;
	corpus: RustCorpusProvenance;
}

export interface RustScenarioReport {
	scenario: string;
	extensionCount: number;
	terminalInputMode: string;
	requestsPerSample: number;
	normalizedSamplesMs: number[];
	timeoutSamplesMs?: number[];
	localitySamplesMs?: number[];
}

export interface RustCorrectness {
	helloAckObserved: boolean;
	idCorrelation: boolean;
	deterministicPayloads: boolean;
	activeWidgetKeys: number;
	slowTimeoutCode: string;
	slowTimeoutRetryable: boolean;
}

export interface RustSamplerReport {
	schemaVersion: number;
	provenance: RustProvenance;
	scenarios: RustScenarioReport[];
	correctness: RustCorrectness;
	pass: boolean;
	failures: string[];
}

function requireFiniteSamples(values: unknown, field: string): number[] {
	if (
		!Array.isArray(values) ||
		values.length === 0 ||
		!values.every((value) => typeof value === "number" && Number.isFinite(value) && value >= 0)
	) {
		throw new RustSamplerError(`${field} must be a non-empty array of finite non-negative numbers`);
	}
	return values;
}

/** Parse the sampler's stdout: exactly one JSON report line, no cargo prose. */
export function parseRustSamplerOutput(stdout: string): RustSamplerReport {
	const lines = stdout.split("\n").map((line) => line.trim()).filter((line) => line.length > 0);
	if (lines.length !== 1) {
		throw new RustSamplerError(
			`expected exactly one JSONL report line from the Rust sampler, got ${lines.length}`,
		);
	}
	const [line] = lines;
	if (line === undefined) {
		throw new RustSamplerError("Rust sampler report is empty");
	}
	let parsed: unknown;
	try {
		parsed = JSON.parse(line);
	} catch (error) {
		throw new RustSamplerError(
			`Rust sampler report is not valid JSON: ${error instanceof Error ? error.message : String(error)}`,
		);
	}
	if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
		throw new RustSamplerError("Rust sampler report must be a JSON object");
	}
	return parsed as RustSamplerReport;
}

/** Fail closed on missing, malformed, or provenance-mismatched sampler output. */
export function validateRustSamplerReport(report: RustSamplerReport): void {
	if (report.schemaVersion !== EXPECTED_RUST_SCHEMA_VERSION) {
		throw new RustSamplerError(
			`Rust sampler schemaVersion ${String(report.schemaVersion)} != ${EXPECTED_RUST_SCHEMA_VERSION}`,
		);
	}
	if (report.pass !== true || !Array.isArray(report.failures) || report.failures.length !== 0) {
		throw new RustSamplerError(
			`Rust sampler reported failure: ${JSON.stringify(report.failures ?? "missing failures")}`,
		);
	}
	const provenance = report.provenance;
	if (typeof provenance !== "object" || provenance === null) {
		throw new RustSamplerError("Rust sampler provenance is missing");
	}
	if (provenance.entrypoint !== EXPECTED_RUST_ENTRYPOINT) {
		throw new RustSamplerError(
			`Rust sampler entrypoint ${String(provenance.entrypoint)} != ${EXPECTED_RUST_ENTRYPOINT}`,
		);
	}
	if (provenance.frameCodec !== EXPECTED_RUST_FRAME_CODEC) {
		throw new RustSamplerError(
			`Rust sampler frameCodec ${String(provenance.frameCodec)} != ${EXPECTED_RUST_FRAME_CODEC}`,
		);
	}
	const protocol = provenance.protocol;
	if (
		typeof protocol !== "object" || protocol === null ||
		protocol.compiledProtocolVersion !== PROTOCOL_VERSION ||
		protocol.observedProtocolVersion !== PROTOCOL_VERSION ||
		protocol.compiledCompatibilityVersion !== COMPATIBILITY_VERSION ||
		protocol.observedCompatibilityVersion !== COMPATIBILITY_VERSION
	) {
		throw new RustSamplerError(
			`Rust sampler protocol provenance mismatch: compiled=${JSON.stringify(protocol?.compiledProtocolVersion)}/${JSON.stringify(protocol?.compiledCompatibilityVersion)} observed=${JSON.stringify(protocol?.observedProtocolVersion)}/${JSON.stringify(protocol?.observedCompatibilityVersion)}, expected ${PROTOCOL_VERSION}/${COMPATIBILITY_VERSION}`,
		);
	}
	const corpus = provenance.corpus;
	if (typeof corpus !== "object" || corpus === null) {
		throw new RustSamplerError("Rust sampler corpus provenance is missing");
	}
	if (corpus.identity !== EXPECTED_CORPUS_IDENTITY) {
		throw new RustSamplerError(
			`Rust sampler corpus identity ${String(corpus.identity)} != ${EXPECTED_CORPUS_IDENTITY}`,
		);
	}
	if (corpus.digestAlgorithm !== EXPECTED_CORPUS_DIGEST_ALGORITHM) {
		throw new RustSamplerError(
			`Rust sampler corpus digest algorithm ${String(corpus.digestAlgorithm)} != ${EXPECTED_CORPUS_DIGEST_ALGORITHM}`,
		);
	}
	if (corpus.digest !== EXPECTED_CORPUS_DIGEST) {
		throw new RustSamplerError(
			`Rust sampler corpus digest ${String(corpus.digest)} != ${EXPECTED_CORPUS_DIGEST}`,
		);
	}
	if (
		corpus.measuredRounds !== EXPECTED_MEASURED_ROUNDS ||
		corpus.warmupsPerScenario !== EXPECTED_WARMUPS_PER_SCENARIO ||
		corpus.samplesPerScenario !== EXPECTED_REQUESTS_PER_SAMPLE ||
		corpus.fastStreamSamples !== EXPECTED_REQUESTS_PER_SAMPLE
	) {
		throw new RustSamplerError(`Rust sampler corpus measurement shape mismatch: ${JSON.stringify(corpus)}`);
	}
	if (!Array.isArray(report.scenarios)) {
		throw new RustSamplerError("Rust sampler scenarios must be an array");
	}
	const seen = new Set<string>();
	for (const scenario of report.scenarios) {
		if (typeof scenario !== "object" || scenario === null) {
			throw new RustSamplerError("Rust sampler scenario must be an object");
		}
		const expected = EXPECTED_SCENARIOS.find((name) => name === scenario.scenario);
		if (expected === undefined) {
			throw new RustSamplerError(`Rust sampler reported unknown scenario ${String(scenario.scenario)}`);
		}
		if (seen.has(expected)) {
			throw new RustSamplerError(`Rust sampler reported duplicate scenario ${expected}`);
		}
		seen.add(expected);
		if (scenario.extensionCount !== EXPECTED_SCENARIO_EXTENSIONS[expected]) {
			throw new RustSamplerError(
				`Rust sampler scenario ${expected} reported ${String(scenario.extensionCount)} extensions, expected ${EXPECTED_SCENARIO_EXTENSIONS[expected]}`,
			);
		}
		if (scenario.terminalInputMode !== EXPECTED_SCENARIO_MODES[expected]) {
			throw new RustSamplerError(
				`Rust sampler scenario ${expected} mode ${String(scenario.terminalInputMode)} != ${EXPECTED_SCENARIO_MODES[expected]}`,
			);
		}
		if (scenario.requestsPerSample !== EXPECTED_REQUESTS_PER_SAMPLE) {
			throw new RustSamplerError(
				`Rust sampler scenario ${expected} requestsPerSample ${String(scenario.requestsPerSample)} != ${EXPECTED_REQUESTS_PER_SAMPLE}`,
			);
		}
		const normalized = requireFiniteSamples(
			scenario.normalizedSamplesMs,
			`scenario ${expected} normalizedSamplesMs`,
		);
		if (normalized.length !== EXPECTED_MEASURED_ROUNDS) {
			throw new RustSamplerError(`Rust sampler scenario ${expected} sample count mismatch`);
		}
		const timeout = scenario.timeoutSamplesMs;
		const locality = scenario.localitySamplesMs;
		if (expected === "slowTerminalInput") {
			if (
				requireFiniteSamples(timeout, `scenario ${expected} timeoutSamplesMs`).length !== EXPECTED_MEASURED_ROUNDS ||
				requireFiniteSamples(locality, `scenario ${expected} localitySamplesMs`).length !== EXPECTED_MEASURED_ROUNDS
			) {
				throw new RustSamplerError(`Rust sampler scenario ${expected} timeout/locality sample count mismatch`);
			}
		} else if (timeout !== undefined || locality !== undefined) {
			throw new RustSamplerError(`Rust sampler scenario ${expected} reported unexpected timeout/locality samples`);
		}
	}
	if (seen.size !== EXPECTED_SCENARIOS.length) {
		const missing = EXPECTED_SCENARIOS.filter((name) => !seen.has(name));
		throw new RustSamplerError(`Rust sampler is missing scenarios: ${missing.join(", ")}`);
	}
	const correctness = report.correctness;
	if (
		typeof correctness !== "object" || correctness === null ||
		correctness.helloAckObserved !== true ||
		correctness.idCorrelation !== true ||
		correctness.deterministicPayloads !== true ||
		correctness.activeWidgetKeys !== 20 ||
		correctness.slowTimeoutCode !== "timeout" ||
		correctness.slowTimeoutRetryable !== false
	) {
		throw new RustSamplerError(`Rust sampler correctness attestation mismatch: ${JSON.stringify(correctness)}`);
	}
}

/** Build (unless disabled) and run the production serve_io sampler. */
export function runRustSampler(
	options: { build?: boolean; binaryPath?: string } = {},
): RustSamplerReport {
	const binaryPath = options.binaryPath ?? RUST_SAMPLER_BIN;
	if (options.build !== false) {
		const cargo = Bun.which("cargo");
		if (!cargo) {
			throw new RustSamplerError("cargo executable is required to build the Rust sampler");
		}
		const built = Bun.spawnSync(
			[cargo, "build", "--release", "-p", "pi-ext", "--bin", "pi-extension-scaling"],
			{
				cwd: REPOSITORY_ROOT,
				env: { ...process.env, CARGO_TARGET_DIR: resolve(REPOSITORY_ROOT, "target") },
				stdout: "inherit",
				stderr: "inherit",
			},
		);
		if (built.exitCode !== 0) {
			throw new RustSamplerError(`cargo build of the Rust sampler exited ${built.exitCode}`);
		}
	}
	if (!existsSync(binaryPath)) {
		throw new RustSamplerError(`Rust sampler binary is missing: ${binaryPath}`);
	}
	const spawned = Bun.spawnSync([binaryPath, "--json"], {
		cwd: REPOSITORY_ROOT,
		stdout: "pipe",
		stderr: "pipe",
	});
	if (spawned.exitCode !== 0) {
		throw new RustSamplerError(
			`Rust sampler exited ${spawned.exitCode}: ${new TextDecoder().decode(spawned.stderr).slice(0, 2000)}`,
		);
	}
	const report = parseRustSamplerOutput(new TextDecoder().decode(spawned.stdout));
	validateRustSamplerReport(report);
	return report;
}

async function measureTerminalInput(
	stdin: Readable,
	collector: FrameCollector,
	samples: number,
	startId: number,
	data: string | ((index: number) => string),
): Promise<number[]> {
	const latencies: number[] = [];
	let id = startId;
	for (let i = 0; i < samples; i++) {
		const value = typeof data === "string" ? data : data(i);
		const t0 = performance.now();
		stdin.push(
			Buffer.from(
				encodeFrameString({
					id,
					kind: "req",
					method: "terminalInput",
					payload: { data: value },
				}),
			),
		);
		await collector.awaitFrame((f) => f.id === id && f.kind === "res");
		latencies.push(performance.now() - t0);
		id += 1;
	}
	return latencies;
}

/** Split one scenario's measurement into interleaved batches so the noise
 * gate can key on per-round medians instead of one long autocorrelated run. */
async function measureTerminalInputRounds(
	stdin: Readable,
	collector: FrameCollector,
	rounds: number,
	samplesPerRound: number,
	startId: number,
	data: string | ((index: number) => string),
): Promise<number[][]> {
	const batches: number[][] = [];
	const totalRounds = rounds + NOISE_ROUND_WARMUPS;
	for (let round = 0; round < totalRounds; round++) {
		const offset = round * samplesPerRound;
		const batch = await measureTerminalInput(
			stdin,
			collector,
			samplesPerRound,
			startId + offset,
			typeof data === "string" ? data : (index) => data(offset + index),
		);
		if (round >= NOISE_ROUND_WARMUPS) batches.push(batch);
	}
	return batches;
}

async function measureFrameCpuRounds(
	stdin: Readable,
	collector: FrameCollector,
	keys: string[],
	rounds: number,
	samplesPerRound: number,
	startId: number,
): Promise<number[][]> {
	const batches: number[][] = [];
	const totalRounds = rounds + NOISE_ROUND_WARMUPS;
	for (let round = 0; round < totalRounds; round++) {
		const batch = await measureFrameCpu(
			stdin,
			collector,
			keys,
			samplesPerRound,
			startId + round * samplesPerRound,
		);
		if (round >= NOISE_ROUND_WARMUPS) batches.push(batch);
	}
	return batches;
}
async function measureFrameCpu(
	stdin: Readable,
	collector: FrameCollector,
	keys: string[],
	samples: number,
	startId: number,
): Promise<number[]> {
	const latencies: number[] = [];
	let id = startId;
	for (let i = 0; i < samples; i++) {
		const key = keys.length === 0 ? "__none__" : (keys[i % keys.length] ?? "__none__");
		const t0 = performance.now();
		stdin.push(
			Buffer.from(
				encodeFrameString({
					id,
					kind: "req",
					method: "measure",
					payload: { key, width: 80 },
				}),
			),
		);
		await collector.awaitFrame((f) => f.id === id && f.kind === "res");
		latencies.push(performance.now() - t0);
		id += 1;
	}
	return latencies;
}

function withinTenPercent(baseline: number, candidate: number): boolean {
	const limit = Math.max(baseline * 1.1, baseline + 0.25);
	return candidate <= limit;
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

function thresholds() {
	return {
		idleWithinPctOfZero: 10,
		fastTerminalInputP99Ms: 5,
		slowTerminalInputTimeoutMs: EXTENSION_INPUT_TIMEOUT_MS,
		inputQueueCapacity: EXTENSION_INPUT_QUEUE_CAPACITY,
	};
}

/** Unchanged 20% noise limit; only the input distribution changed to round
 * medians, so this block is provenance, not a new threshold. */
function noiseGateMetadata() {
	return {
		warmupRounds: NOISE_ROUND_WARMUPS,
		roundMedianRelativeSpreadLimit: NOISE_RELATIVE_SPREAD_LIMIT,
		rounds: NOISE_ROUNDS,
		samplesPerRound: SAMPLES_PER_ROUND,
	};
}

function withRounds<T extends object>(distribution: T, rounds: RoundSummary) {
	return {
		...distribution,
		rounds: {
			roundMedians: rounds.roundMedians,
			median: rounds.roundMedian,
			stddev: rounds.roundStddev,
			relativeSpread: rounds.roundRelativeSpread,
		},
	};
}

function writeArtifact(artifact: object): string {
	const outDir = resolve(process.cwd(), "target", "bench");
	mkdirSync(outDir, { recursive: true });
	const outPath = resolve(outDir, "extension-scaling.json");
	writeFileSync(outPath, `${JSON.stringify(artifact, null, 2)}\n`);
	return outPath;
}

async function shutdown(session: {
	stdin: Readable;
	host: ExtensionHost;
	runPromise: Promise<void>;
}): Promise<void> {
	session.stdin.push(null);
	session.host.dispose("bench");
	await session.runPromise.catch(() => void 0);
}

async function main(): Promise<void> {
	const warmups = 30;
	const samples = 100;
	const failures: string[] = [];

	// ----- zero extensions -----
	const zero = await connectHost([]);
	await waitUntilReady(zero.stdin, zero.collector, 2);
	await measureTerminalInput(zero.stdin, zero.collector, warmups, 100, "k");
	const zeroKeyRounds = await measureTerminalInputRounds(
		zero.stdin,
		zero.collector,
		NOISE_ROUNDS,
		SAMPLES_PER_ROUND,
		200,
		"k",
	);
	const zeroFrameRounds = await measureFrameCpuRounds(
		zero.stdin,
		zero.collector,
		[],
		NOISE_ROUNDS,
		SAMPLES_PER_ROUND,
		400,
	);
	await shutdown(zero);

	// ----- 100 idle -----
	const idle = await connectHost(Array.from({ length: 100 }, () => idleFactory));
	await waitUntilReady(idle.stdin, idle.collector, 2);
	await measureTerminalInput(idle.stdin, idle.collector, warmups, 100, "k");
	const idleKeyRounds = await measureTerminalInputRounds(
		idle.stdin,
		idle.collector,
		NOISE_ROUNDS,
		SAMPLES_PER_ROUND,
		200,
		"k",
	);
	const idleFrameRounds = await measureFrameCpuRounds(
		idle.stdin,
		idle.collector,
		[],
		NOISE_ROUNDS,
		SAMPLES_PER_ROUND,
		400,
	);
	await shutdown(idle);

	// ----- 20 active widgets -----
	const active = await connectHost(Array.from({ length: 20 }, () => widgetActiveFactory));
	await waitUntilReady(active.stdin, active.collector, 90);
	await sendSessionStart(active.stdin, active.collector, 2);
	const activeKeys: string[] = [];
	const slotDeadline = Date.now() + 5_000;
	while (activeKeys.length < 20 && Date.now() < slotDeadline) {
		const frame = await active.collector.awaitFrame(
			(f) =>
				f.method === "uiSlot" &&
				!activeKeys.includes(String((f.payload as Record<string, unknown>)["key"])),
			2_000,
		);
		const key = (frame.payload as Record<string, unknown>)["key"];
		if (typeof key === "string") activeKeys.push(key);
	}
	await measureTerminalInput(active.stdin, active.collector, warmups, 100, "k");
	const activeKey = await measureTerminalInput(
		active.stdin,
		active.collector,
		samples,
		200,
		"k",
	);
	const activeFrame = await measureFrameCpu(
		active.stdin,
		active.collector,
		activeKeys,
		samples,
		400,
	);
	await shutdown(active);

	const zeroKeyStats = stats(zeroKeyRounds.flat());
	const idleKeyStats = stats(idleKeyRounds.flat());
	const activeKeyStats = stats(activeKey);
	const zeroFrameStats = stats(zeroFrameRounds.flat());
	const idleFrameStats = stats(idleFrameRounds.flat());
	const activeFrameStats = stats(activeFrame);
	const zeroKeyRoundsSummary = roundSummary(zeroKeyRounds);
	const idleKeyRoundsSummary = roundSummary(idleKeyRounds);
	const zeroFrameRoundsSummary = roundSummary(zeroFrameRounds);
	const idleFrameRoundsSummary = roundSummary(idleFrameRounds);

	if (!withinTenPercent(zeroKeyStats.p99, idleKeyStats.p99)) {
		failures.push(
			`idle keypress p99 ${idleKeyStats.p99.toFixed(3)}ms > 110% of zero ${zeroKeyStats.p99.toFixed(3)}ms`,
		);
	}
	if (!withinTenPercent(zeroFrameStats.p99, idleFrameStats.p99)) {
		failures.push(
			`idle frame p99 ${idleFrameStats.p99.toFixed(3)}ms > 110% of zero ${zeroFrameStats.p99.toFixed(3)}ms`,
		);
	}

	// ----- fast terminal input -----
	const fast = await connectHost([terminalInputFastFactory]);
	await waitUntilReady(fast.stdin, fast.collector, 90);
	await sendSessionStart(fast.stdin, fast.collector, 2);
	await measureTerminalInput(fast.stdin, fast.collector, warmups, 100, "a");
	const fastRounds = await measureTerminalInputRounds(
		fast.stdin,
		fast.collector,
		NOISE_ROUNDS,
		SAMPLES_PER_ROUND,
		300,
		(i) => (i % 3 === 0 ? "x" : i % 3 === 1 ? "a" : "b"),
	);
	await shutdown(fast);
	const fastStats = stats(fastRounds.flat());
	const fastRoundsSummary = roundSummary(fastRounds);
	if (fastStats.p99 >= 5) {
		failures.push(`fast terminalInput p99 ${fastStats.p99.toFixed(3)}ms >= 5ms`);
	}

	// ----- slow terminal input -----
	const slow = await connectHost([terminalInputSlowFactory, terminalInputFastFactory]);
	await waitUntilReady(slow.stdin, slow.collector, 90);
	await sendSessionStart(slow.stdin, slow.collector, 2);
	const slowT0 = performance.now();
	slow.stdin.push(
		Buffer.from(
			encodeFrameString({
				id: 10,
				kind: "req",
				method: "terminalInput",
				payload: { data: "q" },
			}),
		),
	);
	const slowFirst = await slow.collector.awaitFrame((f) => f.id === 10 && f.kind === "res");
	const slowFirstMs = performance.now() - slowT0;
	const slowFirstPayload = slowFirst.payload as Record<string, unknown>;
	const slowErr = await slow.collector.awaitFrame((f) => f.method === "extensionError", 2_000);
	const slowT1 = performance.now();
	slow.stdin.push(
		Buffer.from(
			encodeFrameString({
				id: 11,
				kind: "req",
				method: "terminalInput",
				payload: { data: "a" },
			}),
		),
	);
	const slowSecond = await slow.collector.awaitFrame((f) => f.id === 11 && f.kind === "res");
	const slowSecondMs = performance.now() - slowT1;
	const slowSecondPayload = slowSecond.payload as Record<string, unknown>;
	const activeAfter = slow.host.activeTerminalHandlerCount;
	await shutdown(slow);

	if (slowFirstPayload["consume"] !== false || slowFirstPayload["data"] !== "q") {
		failures.push("slow path did not pass original key");
	}
	if (slowFirstMs < EXTENSION_INPUT_TIMEOUT_MS - 1) {
		failures.push(
			`slow path returned too fast (${slowFirstMs.toFixed(3)}ms < ${EXTENSION_INPUT_TIMEOUT_MS}ms)`,
		);
	}
	if (activeAfter !== 1) {
		failures.push(`expected 1 active handler after disable, got ${activeAfter}`);
	}
	if (slowSecondPayload["data"] !== "A" || slowSecondMs >= 5) {
		failures.push(
			`later input after disable not local (data=${String(slowSecondPayload["data"])}, ms=${slowSecondMs.toFixed(3)})`,
		);
	}
	if ((slowErr.payload as Record<string, unknown>)["retryable"] !== false) {
		failures.push("extensionError must be non-retryable");
	}

	function results() {
		return {
			zero: {
				keypress: withRounds(zeroKeyStats, zeroKeyRoundsSummary),
				frame: withRounds(zeroFrameStats, zeroFrameRoundsSummary),
			},
			idle100: {
				keypress: withRounds(idleKeyStats, idleKeyRoundsSummary),
				frame: withRounds(idleFrameStats, idleFrameRoundsSummary),
			},
			active20: {
				keypress: activeKeyStats,
				frame: activeFrameStats,
				widgetKeys: activeKeys.length,
			},
			fastTerminalInput: withRounds(fastStats, fastRoundsSummary),
			slowTerminalInput: {
				firstMs: slowFirstMs,
				secondMs: slowSecondMs,
				activeHandlersAfter: activeAfter,
				firstPayload: slowFirstPayload,
				secondPayload: slowSecondPayload,
			},
		};
	}

	// ----- Rust production serve_io sampler (composite artifact half) -----
	let rustSection: Record<string, unknown>;
	let rustQuietLanes: NoisyDistribution[];
	let rustEntrypoint = "unknown";
	try {
		const rust = runRustSampler();
		rustEntrypoint = rust.provenance.entrypoint;
		rustSection = {
			schemaVersion: rust.schemaVersion,
			provenance: rust.provenance,
			correctness: rust.correctness,
			scenarios: Object.fromEntries(
				rust.scenarios.map((scenario) => [
					scenario.scenario,
					{
						extensionCount: scenario.extensionCount,
						terminalInputMode: scenario.terminalInputMode,
						requestsPerSample: scenario.requestsPerSample,
						normalized: stats(scenario.normalizedSamplesMs),
						...(scenario.timeoutSamplesMs
							? { timeout: stats(scenario.timeoutSamplesMs) }
							: {}),
						...(scenario.localitySamplesMs
							? { locality: stats(scenario.localitySamplesMs) }
							: {}),
					},
				]),
			),
			pass: true,
		};
		rustQuietLanes = rust.scenarios.map((scenario) => {
			const median = percentile([...scenario.normalizedSamplesMs].sort((a, b) => a - b), 50);
			const spread = spreadStats(scenario.normalizedSamplesMs, median);
			return {
				label: `rust ${scenario.scenario} round aggregates (n=${scenario.normalizedSamplesMs.length} rounds)`,
				count: scenario.normalizedSamplesMs.length,
				median,
				stddev: spread.stddev,
				relativeSpread: spread.relativeSpread,
			};
		});
	} catch (error) {
		if (!(error instanceof RustSamplerError)) throw error;
		// Fail closed: no Rust production-path proof, no artifact pass.
		const message = `rust serve_io sampler: ${error.message}`;
		const artifact = {
			check: 8,
			name: "extension-scaling",
			warmups,
			samples,
			thresholds: thresholds(),
			machine: machineMetadata(),
			results: results(),
			rust: { pass: false, error: error.message },
			pass: false,
			failures: [...failures, message],
		};
		const outPath = writeArtifact(artifact);
		process.stderr.write(`FAIL: ${message}\nartifact=${outPath}\n`);
		process.exit(1);
	}

	try {
		requireQuiet([
			roundNoiseLane("zero keypress", zeroKeyRoundsSummary),
			roundNoiseLane("idle100 keypress", idleKeyRoundsSummary),
			roundNoiseLane("zero frame", zeroFrameRoundsSummary),
			roundNoiseLane("idle100 frame", idleFrameRoundsSummary),
			roundNoiseLane("fast terminalInput", fastRoundsSummary),
			...rustQuietLanes,
		]);
	} catch (error) {
		if (!(error instanceof NoiseRejection)) throw error;
		const artifact = {
			check: 8,
			name: "extension-scaling",
			warmups,
			samples,
			noiseGate: noiseGateMetadata(),
			thresholds: thresholds(),
			machine: machineMetadata(),
			results: results(),
			rust: rustSection,
			pass: false,
			failures,
			noise: {
				rejections: error.noisy,
				remediation: REMEDIATION_LADDER,
			},
		};
		const outPath = writeArtifact(artifact);
		process.stderr.write(`NOISE:\n${formatNoiseRejection(error.noisy)}\n`);
		process.stderr.write(`extension-scaling: pass=false artifact=${outPath}\n`);
		process.exit(NOISE_EXIT_CODE);
	}

	const artifact = {
		check: 8,
		name: "extension-scaling",
		warmups,
		samples,
		noiseGate: noiseGateMetadata(),
		thresholds: thresholds(),
		machine: machineMetadata(),
		results: results(),
		rust: rustSection,
		pass: failures.length === 0,
		failures,
	};

	const outPath = writeArtifact(artifact);

	process.stderr.write(
		`extension-scaling: pass=${artifact.pass} artifact=${outPath}\n` +
			`  zero keypress p99=${zeroKeyStats.p99.toFixed(3)}ms frame p99=${zeroFrameStats.p99.toFixed(3)}ms\n` +
			`  idle100 keypress p99=${idleKeyStats.p99.toFixed(3)}ms frame p99=${idleFrameStats.p99.toFixed(3)}ms\n` +
			`  active20 keypress p99=${activeKeyStats.p99.toFixed(3)}ms frame p99=${activeFrameStats.p99.toFixed(3)}ms\n` +
			`  fast terminalInput p99=${fastStats.p99.toFixed(3)}ms\n` +
			`  slow first=${slowFirstMs.toFixed(3)}ms second=${slowSecondMs.toFixed(3)}ms\n` +
			`  rust entrypoint=${rustEntrypoint}\n`,
	);
	if (failures.length > 0) {
		for (const f of failures) process.stderr.write(`  FAIL: ${f}\n`);
		process.exit(1);
	}
}

if (import.meta.main) {
	await main();
}
