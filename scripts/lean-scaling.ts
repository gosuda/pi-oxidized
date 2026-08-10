/**
 * Mode-2 distinctness gate: child-process startup benchmark comparing the
 * compat (Mode 1) and lean (Mode 2) extension host, plus a per-call contract
 * proof that lean keeps tool.prepare/tool.validate/tool.execute as real RPCs.
 *
 * Both modes spawn the SAME host entry (`bun src/main.ts` inside
 * packages/extension-host); only `--lean` and the fixture entry differ. Each
 * sample measures wall time from process spawn to the hello response and to
 * the extensions.load terminal response — synchronized on protocol frames,
 * never on bytes or timers. Every child is reaped exactly once.
 *
 * The gate is a same-run RELATIVE comparison (lean p50 / compat p50), so it
 * is hardware-independent; absolute times are recorded only for context.
 */
import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import {
	COMPATIBILITY_VERSION,
	type Frame,
	PROTOCOL_VERSION,
	encodeFrameString,
} from "../packages/pi-tui-protocol/src/index.ts";

/** Default warmups discarded per mode before sampling. */
export const DEFAULT_WARMUPS = 5;
/** Default measured samples per mode. */
export const DEFAULT_SAMPLES = 15;
/**
 * Default gate: lean p50 must be at most 85% of compat p50 in the same run.
 * The real margin is far larger (lean skips the entire upstream module graph
 * and jiti), so a 15% required margin is conservative against CI jitter while
 * still proving the modes are observably distinct.
 */
export const DEFAULT_LEAN_MAX_RATIO = 0.85;
/** Per-protocol-step deadline; only trips when a child wedges. */
const DEFAULT_STEP_TIMEOUT_MS = 20_000;

// ---------------------------------------------------------------------------
// Pure statistics (unit-tested)
// ---------------------------------------------------------------------------

export interface StatSummary {
	n: number;
	min: number;
	median: number;
	p95: number;
	max: number;
	mean: number;
}

export function percentile(sorted: number[], p: number): number {
	if (sorted.length === 0) return 0;
	const idx = Math.min(
		sorted.length - 1,
		Math.max(0, Math.ceil((p / 100) * sorted.length) - 1),
	);
	return sorted[idx] ?? 0;
}

export function summarize(samples: number[]): StatSummary {
	const sorted = [...samples].sort((a, b) => a - b);
	const mean =
		sorted.length === 0 ? 0 : sorted.reduce((acc, v) => acc + v, 0) / sorted.length;
	return {
		n: sorted.length,
		min: sorted[0] ?? 0,
		median: percentile(sorted, 50),
		p95: percentile(sorted, 95),
		max: sorted[sorted.length - 1] ?? 0,
		mean,
	};
}

export interface DistinctnessVerdict {
	/** lean p50 / compat p50 (Infinity when compat p50 is not positive). */
	ratio: number;
	/** Gate outcome; always true when not enforced. */
	pass: boolean;
	/** Whether a max ratio was supplied. */
	enforced: boolean;
}

export function evaluateDistinctness(
	compatP50: number,
	leanP50: number,
	maxRatio: number | undefined,
): DistinctnessVerdict {
	const ratio = compatP50 > 0 ? leanP50 / compatP50 : Number.POSITIVE_INFINITY;
	if (maxRatio === undefined) return { ratio, pass: true, enforced: false };
	const pass =
		compatP50 > 0 && Number.isFinite(leanP50) && leanP50 >= 0 && ratio <= maxRatio;
	return { ratio, pass, enforced: true };
}

export interface ZeroIdleExtensionMetrics {
	readonly keypressP99: number;
	readonly frameP99: number;
}

function withinTenPercent(baseline: number, candidate: number): boolean {
	const limit = Math.max(baseline * 1.1, baseline + 0.25);
	return candidate <= limit;
}

/** Report zero-vs-idle regressions for both extension-host hot paths. */
export function evaluateZeroIdleSanity(
	zero: ZeroIdleExtensionMetrics,
	idle: ZeroIdleExtensionMetrics,
): string[] {
	const failures: string[] = [];
	if (!withinTenPercent(zero.keypressP99, idle.keypressP99)) {
		failures.push(
			`idle keypress p99 ${idle.keypressP99.toFixed(3)}ms > 110% of zero ${zero.keypressP99.toFixed(3)}ms`,
		);
	}
	if (!withinTenPercent(zero.frameP99, idle.frameP99)) {
		failures.push(
			`idle frame p99 ${idle.frameP99.toFixed(3)}ms > 110% of zero ${zero.frameP99.toFixed(3)}ms`,
		);
	}
	return failures;
}

// ---------------------------------------------------------------------------
// Bounded child-process JSONL driver
// ---------------------------------------------------------------------------

/** Handle returned by the global timer APIs used for wait/race timeouts. */
export type TimerHandle = ReturnType<typeof setTimeout>;

interface Waiter {
	predicate: (frame: Frame) => boolean;
	resolve: (frame: Frame) => void;
	reject: (error: Error) => void;
	timer: TimerHandle;
}

interface WaitHandle {
	promise: Promise<Frame>;
	rejectAndRemove(error: Error): void;
}

/** Bound for stdout-line prefixes embedded in parse-failure diagnostics. */
const DIAGNOSTIC_STDOUT_PREFIX_CHARS = 512;
/**
 * Cap on the incomplete stdout line held in `ChildHost.buffer`.
 * Matches the order of existing bounded-diagnostic constants (KiB-scale);
 * an unterminated line past this limit is treated as a wedged/hostile child.
 */
export const MAX_STDOUT_BUFFER_CHARS = 64 * 1024;
/** Default parsed-frame cap for hosts outside the tool-round-trip workload. */
export const MAX_RETAINED_FRAMES = 128;
/** Absolute cap for retained hostile child output. */
export const DEFAULT_HOSTILE_OUTPUT_CEILING = 10_000;

/**
 * Eight frames per tool round (prepare, validate, update, execute, plus
 * headroom for additional toolUpdate frames per round), plus setup slack.
 * The per-round term is doubled from the strict 4-frame minimum so a round
 * that emits more than one toolUpdate does not trip a false hostile gate.
 * The hard ceiling still bounds hostile child output.
 */
export function deriveRetainedFrameBudget(rounds: number): number {
	return Math.min(
		DEFAULT_HOSTILE_OUTPUT_CEILING,
		Math.max(MAX_RETAINED_FRAMES, 16 + rounds * 8),
	);
}

/**
 * Escape and truncate hostile text so diagnostics stay bounded and printable.
 * JSON string encoding keeps control bytes / quotes from corrupting the message.
 */
function diagnosticSnippet(text: string, maxChars: number): string {
	const truncated = text.length > maxChars ? `${text.slice(0, maxChars)}…` : text;
	return JSON.stringify(truncated);
}

export interface HostSpec {
	/** Working directory for `bun` (packages/extension-host, so its tsconfig applies). */
	hostCwd: string;
	/** Host entry relative to hostCwd ("src/main.ts"). */
	hostEntry: string;
	/** Mode 2 when true; Mode 1 compat otherwise. */
	lean: boolean;
	/** Extension entry loaded through the extensions.load RPC. */
	extensionPath: string;
	/** Per-host retained-frame cap; tool rounds derive this from their workload. */
	maxRetainedFrames?: number;
}

/** One bounded host child: frame pump, request/response correlation, reap-on-close. */
export class ChildHost {
	readonly frames: Frame[] = [];
	private readonly child: ChildProcessWithoutNullStreams;
	private readonly waiters: Waiter[] = [];
	/** Streaming decoder so multibyte UTF-8 sequences may safely span chunks. */
	private readonly decoder = new TextDecoder("utf-8");
	private buffer = "";
	private stderrTail = "";
	private nextId = 1;
	private exited = false;
	private readonly exitPromise: Promise<void>;
	private resolveExit: (() => void) | undefined;
	private fatalError: Error | undefined;
	private pumping = true;
	/** True once we intentionally end stdin or SIGKILL the child; marks in-flight write errors as expected. */
	private tearingDown = false;
	private readonly maxRetainedFrames: number;

	constructor(spec: HostSpec) {
		this.maxRetainedFrames = spec.maxRetainedFrames ?? MAX_RETAINED_FRAMES;
		const args = [spec.hostEntry];
		if (spec.lean) args.push("--lean");
		args.push("--cwd", spec.hostCwd);
		const { promise, resolve } = Promise.withResolvers<void>();
		this.exitPromise = promise;
		this.resolveExit = resolve;
		this.child = spawn(process.execPath, args, {
			cwd: spec.hostCwd,
			env: process.env,
			stdio: ["pipe", "pipe", "pipe"],
		});
		this.child.stdout.on("data", (chunk: Buffer) => this.onData(chunk));
		// A killed/wedged child turns in-flight writes into async EPIPE/close
		// events on stdin. Without a listener the stream rethrows and kills the
		// harness; own them explicitly: expected races (after teardown/exit) are
		// dropped, a live-child write failure is routed through failAll so the
		// pending request rejects cleanly instead of crashing the process.
		this.child.stdin.on("error", (err: NodeJS.ErrnoException) => {
			if (this.tearingDown || this.exited || this.fatalError !== undefined) return;
			this.failAll(
				new Error(`host child stdin write failed: ${err.message}`),
			);
		});
		this.child.stderr.on("data", (chunk: Buffer) => {
			this.stderrTail = (this.stderrTail + chunk.toString()).slice(-4096);
		});
		this.child.once("error", (err) => {
			this.exited = true;
			this.resolveExit?.();
			this.failAll(new Error(`host child failed to spawn: ${err.message}`));
		});
		this.child.once("exit", (code, signal) => {
			this.exited = true;
			this.resolveExit?.();
			this.failAll(
				new Error(
					`host child exited early (code=${String(code)}, signal=${String(signal)}); stderr: ${this.stderrTail}`,
				),
			);
		});
	}


	private onData(chunk: Buffer): void {
		if (!this.pumping) return;
		try {
			// `{ stream: true }` retains an incomplete trailing multibyte sequence
			// inside the decoder across chunk boundaries (unlike Buffer#toString).
			this.buffer += this.decoder.decode(chunk, { stream: true });
			const lines = this.buffer.split("\n");
			this.buffer = lines.pop() ?? "";
			if (this.buffer.length > MAX_STDOUT_BUFFER_CHARS) {
				this.failAll(
					this.stdoutError(
						`host stdout unterminated line exceeded ${String(MAX_STDOUT_BUFFER_CHARS)} characters`,
						this.buffer,
					),
				);
				return;
			}
			for (const line of lines) {
				if (line.trim().length === 0) continue;
				let frame: Frame;
				try {
					frame = JSON.parse(line) as Frame;
				} catch (err) {
					const parseMessage = diagnosticSnippet(
						err instanceof Error ? err.message : String(err),
						DIAGNOSTIC_STDOUT_PREFIX_CHARS,
					);
					this.failAll(
						this.stdoutError(`failed to parse host stdout JSON: ${parseMessage}`, line),
					);
					return;
				}
				if (this.frames.length >= this.maxRetainedFrames) {
					this.failAll(
						this.stdoutError(
							`host retained frame limit exceeded ${String(this.maxRetainedFrames)}`,
							line,
						),
					);
					return;
				}
				this.frames.push(frame);
				for (let i = this.waiters.length - 1; i >= 0; i--) {
					const waiter = this.waiters[i];
					if (waiter !== undefined && waiter.predicate(frame)) {
						if (this.removeWaiter(waiter)) waiter.resolve(frame);
					}
				}
			}
		} catch (err) {
			// Stream listeners must never throw into the event loop.
			this.failAll(err instanceof Error ? err : new Error(String(err)));
		}
	}

	private stdoutError(message: string, stdout: string): Error {
		return new Error(
			`${message}; stdout: ${diagnosticSnippet(stdout, DIAGNOSTIC_STDOUT_PREFIX_CHARS)}; ` +
				`stderr: ${diagnosticSnippet(this.stderrTail, 4096)}`,
		);
	}

	/** Best-effort SIGKILL so a hostile/wedged child cannot keep feeding stdout. */
	private reapChild(): void {
		this.tearingDown = true;
		if (this.exited) return;
		try {
			this.child.kill("SIGKILL");
		} catch {
			// Already gone; exit handler still resolves exitPromise.
		}
	}

	private failAll(error: Error): void {
		if (this.fatalError !== undefined) return;
		this.fatalError = error;
		this.pumping = false;
		this.buffer = "";
		this.reapChild();
		while (this.waiters.length > 0) {
			const waiter = this.waiters.pop();
			if (waiter === undefined) break;
			clearTimeout(waiter.timer);
			waiter.reject(error);
		}
	}

	private removeWaiter(waiter: Waiter): boolean {
		const index = this.waiters.indexOf(waiter);
		if (index === -1) return false;
		this.waiters.splice(index, 1);
		clearTimeout(waiter.timer);
		return true;
	}

	private createWaiter(
		predicate: (frame: Frame) => boolean,
		timeoutMs: number,
		label: string,
	): WaitHandle {
		if (this.fatalError !== undefined) {
			return {
				promise: Promise.reject(this.fatalError),
				rejectAndRemove() {},
			};
		}
		const existing = this.frames.find(predicate);
		if (existing !== undefined) {
			return {
				promise: Promise.resolve(existing),
				rejectAndRemove() {},
			};
		}

		const { promise, resolve, reject } = Promise.withResolvers<Frame>();
		let waiter: Waiter | undefined;
		const rejectAndRemove = (error: Error): void => {
			if (waiter === undefined || !this.removeWaiter(waiter)) return;
			reject(error);
		};
		const timer = setTimeout(() => {
			rejectAndRemove(
				new Error(`timeout waiting for ${label} after ${timeoutMs}ms; stderr: ${this.stderrTail}`),
			);
		}, timeoutMs);
		waiter = { predicate, resolve, reject, timer };
		this.waiters.push(waiter);
		return { promise, rejectAndRemove };
	}

	/** Wait for one matching frame; the timeout is only a wedge guard. */
	waitFor(
		predicate: (frame: Frame) => boolean,
		timeoutMs: number,
		label: string,
	): Promise<Frame> {
		return this.createWaiter(predicate, timeoutMs, label).promise;
	}

	/** Send a request and await its terminal res/error frame (matched by id). */
	async request(method: string, payload: unknown, timeoutMs: number): Promise<Frame> {
		const id = this.nextId++;
		const response = this.createWaiter(
			(f) => f.id === id && (f.kind === "res" || f.kind === "error") && f.method === method,
			timeoutMs,
			`${method} response`,
		);
		try {
			this.child.stdin.write(encodeFrameString({ id, kind: "req", method, payload }));
		} catch (err) {
			const failure = new Error(
				`failed to write ${method}: ${err instanceof Error ? err.message : String(err)}`,
			);
			response.rejectAndRemove(failure);
			void response.promise.catch(() => {});
			throw failure;
		}
		const frame = await response.promise;
		if (frame.kind === "error") {
			throw new Error(`${method} returned error: ${JSON.stringify(frame.payload)}`);
		}
		return frame;
	}

	/** End stdin, then reap: graceful exit first, SIGKILL after the grace window. */
	async close(graceMs = 2_000): Promise<void> {
		if (!this.exited) {
			this.tearingDown = true;
			try {
				this.child.stdin.end();
			} catch {
				// Already broken pipe; the exit path below still reaps.
			}
			let gracefulTimer: TimerHandle | undefined;
			const graceful = await Promise.race([
				this.exitPromise.then(() => true as const),
				new Promise<false>((resolveTimer) => {
					gracefulTimer = setTimeout(() => resolveTimer(false), graceMs);
				}),
			]).finally(() => {
				clearTimeout(gracefulTimer);
			});
			if (!graceful && !this.exited) {
				this.child.kill("SIGKILL");
				let sigkillTimer: TimerHandle | undefined;
				await Promise.race([
					this.exitPromise,
					new Promise<void>((resolveTimer) => {
						sigkillTimer = setTimeout(resolveTimer, 5_000);
					}),
				]).finally(() => {
					clearTimeout(sigkillTimer);
				});
			}
		}
		this.child.stdout.removeAllListeners();
		this.child.stderr.removeAllListeners();
		this.child.stdin.removeAllListeners();
		this.failAll(new Error("host child closed"));
	}
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

export interface ModeSample {
	/** spawn -> hello response. */
	helloMs: number;
	/** hello response -> extensions.load terminal response. */
	loadMs: number;
	/** spawn -> extensions.load terminal response. */
	totalMs: number;
}

export interface ModeStats {
	samples: number;
	helloMs: StatSummary;
	loadMs: StatSummary;
	totalMs: StatSummary;
}

export interface ToolRoundTripResult {
	rounds: number;
	/** toolUpdate events observed across all execute calls. */
	updateEvents: number;
	prepareMs: StatSummary;
	validateMs: StatSummary;
	executeMs: StatSummary;
	totalMs: StatSummary;
}

export interface ModeDistinctnessOptions {
	hostCwd: string;
	hostEntry: string;
	/** Minimal compat fixture (Mode 1 factory entry). */
	compatExtension: string;
	/** Prebundled-style lean fixture (Mode 2 declarative entry). */
	leanExtension: string;
	warmups: number;
	samples: number;
	/** prepare->validate->execute rounds on lean; defaults to `samples`. */
	toolRounds?: number;
	/** Gate: lean p50 / compat p50 must be <= this; omit to measure only. */
	maxRatio?: number;
	stepTimeoutMs?: number;
}

export interface ModeDistinctnessResult {
	warmups: number;
	samples: number;
	maxRatio: number | undefined;
	compat: ModeStats;
	lean: ModeStats;
	verdict: DistinctnessVerdict;
	toolRoundTrip: ToolRoundTripResult;
	failures: string[];
}

async function measureOne(spec: HostSpec, timeoutMs: number): Promise<ModeSample> {
	const t0 = performance.now();
	const host = new ChildHost(spec);
	try {
		const hello = await host.request(
			"hello",
			{ protocolVersion: PROTOCOL_VERSION, compatibilityVersion: COMPATIBILITY_VERSION },
			timeoutMs,
		);
		const t1 = performance.now();
		const helloPayload = hello.payload as Record<string, unknown>;
		if (helloPayload["protocolVersion"] !== PROTOCOL_VERSION) {
			throw new Error(
				`hello ack protocolVersion mismatch: ${JSON.stringify(helloPayload)}`,
			);
		}
		const load = await host.request(
			"extensions.load",
			{ extensionPaths: [spec.extensionPath], cwd: spec.hostCwd },
			timeoutMs,
		);
		const t2 = performance.now();
		const loadPayload = load.payload as Record<string, unknown>;
		const errors = loadPayload["errors"];
		if (Array.isArray(errors) && errors.length > 0) {
			throw new Error(`extensions.load reported errors: ${JSON.stringify(errors)}`);
		}
		return { helloMs: t1 - t0, loadMs: t2 - t1, totalMs: t2 - t0 };
	} finally {
		await host.close();
	}
}

function summarizeMode(samples: ModeSample[]): ModeStats {
	return {
		samples: samples.length,
		helloMs: summarize(samples.map((s) => s.helloMs)),
		loadMs: summarize(samples.map((s) => s.loadMs)),
		totalMs: summarize(samples.map((s) => s.totalMs)),
	};
}

/**
 * Drive tool.prepare -> tool.validate -> tool.execute on one lean child,
 * recording the RTT of each stage separately. Proves Mode 2 keeps all three
 * as real RPC round-trips instead of inlining them away.
 */
async function measureToolRoundTrips(
	spec: HostSpec,
	rounds: number,
	timeoutMs: number,
): Promise<ToolRoundTripResult> {
	const host = new ChildHost({
		...spec,
		maxRetainedFrames: deriveRetainedFrameBudget(rounds),
	});
	const prepareMs: number[] = [];
	const validateMs: number[] = [];
	const executeMs: number[] = [];
	const totalMs: number[] = [];
	try {
		await host.request(
			"hello",
			{ protocolVersion: PROTOCOL_VERSION, compatibilityVersion: COMPATIBILITY_VERSION },
			timeoutMs,
		);
		await host.request(
			"extensions.load",
			{ extensionPaths: [spec.extensionPath], cwd: spec.hostCwd },
			timeoutMs,
		);
		for (let i = 0; i < rounds; i++) {
			const r0 = performance.now();
			const prepared = await host.request(
				"tool.prepare",
				{ name: "echo", args: { text: `probe-${i}` } },
				timeoutMs,
			);
			prepareMs.push(performance.now() - r0);
			const preparedArgs = (prepared.payload as Record<string, unknown>)["args"];
			if (
				(preparedArgs as Record<string, unknown> | undefined)?.["preparedBy"] !== "lean"
			) {
				throw new Error(
					`tool.prepare did not perform real work: ${JSON.stringify(prepared.payload)}`,
				);
			}

			const v0 = performance.now();
			const validated = await host.request(
				"tool.validate",
				{ name: "echo", args: preparedArgs },
				timeoutMs,
			);
			validateMs.push(performance.now() - v0);
			const validatedArgs = (validated.payload as Record<string, unknown>)["args"];
			if (
				(validatedArgs as Record<string, unknown> | undefined)?.["validatedBy"] !== "lean"
			) {
				throw new Error(
					`tool.validate did not perform real work: ${diagnosticSnippet(JSON.stringify(validated.payload), DIAGNOSTIC_STDOUT_PREFIX_CHARS)}`,
				);
			}

			const e0 = performance.now();
			const executed = await host.request(
				"tool.execute",
				{ name: "echo", toolCallId: `bench-${i}`, args: validatedArgs, prepared: true },
				timeoutMs,
			);
			executeMs.push(performance.now() - e0);
			totalMs.push(performance.now() - r0);
			const content = (executed.payload as Record<string, unknown>)["content"];
			if (!Array.isArray(content) || content.length === 0) {
				throw new Error(
					`tool.execute returned no content: ${JSON.stringify(executed.payload)}`,
				);
			}
		}
	} finally {
		await host.close();
	}
	const updateEvents = host.frames.filter((f) => f.method === "toolUpdate").length;
	return {
		rounds,
		updateEvents,
		prepareMs: summarize(prepareMs),
		validateMs: summarize(validateMs),
		executeMs: summarize(executeMs),
		totalMs: summarize(totalMs),
	};
}

/**
 * Interleaved compat/lean spawn benchmark (same host entry, same machine,
 * alternating order so drift hits both modes equally) plus the lean 3-RPC
 * per-call contract proof. Warmup pairs are discarded.
 */
export async function runModeDistinctness(
	options: ModeDistinctnessOptions,
): Promise<ModeDistinctnessResult> {
	const timeoutMs = options.stepTimeoutMs ?? DEFAULT_STEP_TIMEOUT_MS;
	const rounds = options.toolRounds ?? options.samples;
	// Zero/negative rounds would drive no RPCs, so the 3-RPC contract
	// proof (preparedBy/validatedBy markers plus toolUpdate events) would
	// pass without exercising any round-trip.
	if (!Number.isInteger(rounds) || rounds < 1) {
		throw new Error(
			`toolRounds must be a positive integer, got ${String(rounds)}`,
		);
	}
	const compatSpec: HostSpec = {
		hostCwd: options.hostCwd,
		hostEntry: options.hostEntry,
		lean: false,
		extensionPath: options.compatExtension,
	};
	const leanSpec: HostSpec = {
		hostCwd: options.hostCwd,
		hostEntry: options.hostEntry,
		lean: true,
		extensionPath: options.leanExtension,
	};

	const compatSamples: ModeSample[] = [];
	const leanSamples: ModeSample[] = [];
	const iterations = options.warmups + options.samples;
	for (let i = 0; i < iterations; i++) {
		// Alternate which mode spawns first each iteration so machine drift or
		// cache warming cannot systematically favor one side of the ratio.
		const compatFirst = i % 2 === 0;
		const first = await measureOne(compatFirst ? compatSpec : leanSpec, timeoutMs);
		const second = await measureOne(compatFirst ? leanSpec : compatSpec, timeoutMs);
		if (i >= options.warmups) {
			(compatFirst ? compatSamples : leanSamples).push(first);
			(compatFirst ? leanSamples : compatSamples).push(second);
		}
	}

	const compat = summarizeMode(compatSamples);
	const lean = summarizeMode(leanSamples);
	const verdict = evaluateDistinctness(
		compat.totalMs.median,
		lean.totalMs.median,
		options.maxRatio,
	);
	const toolRoundTrip = await measureToolRoundTrips(leanSpec, rounds, timeoutMs);

	const failures: string[] = [];
	if (verdict.enforced && !verdict.pass) {
		failures.push(
			`lean p50 ${lean.totalMs.median.toFixed(1)}ms is not <= ${String(options.maxRatio)}x ` +
				`compat p50 ${compat.totalMs.median.toFixed(1)}ms (ratio ${verdict.ratio.toFixed(3)})`,
		);
	}
	if (toolRoundTrip.updateEvents < rounds) {
		failures.push(
			`lean tool.execute emitted ${toolRoundTrip.updateEvents} toolUpdate events for ${rounds} rounds`,
		);
	}

	return {
		warmups: options.warmups,
		samples: options.samples,
		maxRatio: options.maxRatio,
		compat,
		lean,
		verdict,
		toolRoundTrip,
		failures,
	};
}
