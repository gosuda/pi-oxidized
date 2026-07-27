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
import { mkdirSync, writeFileSync } from "node:fs";
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
	DEFAULT_LEAN_MAX_RATIO,
	DEFAULT_SAMPLES,
	DEFAULT_WARMUPS,
	runModeDistinctness,
} from "./lean-scaling.ts";

const EXTENSION_HOST_PACKAGE = resolve(process.cwd(), "packages", "extension-host");

interface FrameWaiter {
	predicate: (frame: Frame) => boolean;
	resolve: (frame: Frame) => void;
	timer: NodeJS.Timeout;
}

class FrameCollector {
	readonly frames: Frame[] = [];
	private readonly waiters: FrameWaiter[] = [];
	private buf = "";

	write(chunk: Uint8Array): void {
		this.buf += new TextDecoder().decode(chunk);
		const lines = this.buf.split("\n");
		this.buf = lines.pop() ?? "";
		for (const line of lines) {
			if (line.trim().length === 0) continue;
			const frame = JSON.parse(line) as Frame;
			this.frames.push(frame);
			for (let i = this.waiters.length - 1; i >= 0; i--) {
				const waiter = this.waiters[i];
				if (waiter !== undefined && waiter.predicate(frame)) {
					this.waiters.splice(i, 1);
					waiter.resolve(frame);
				}
			}
		}
	}

	awaitFrame(predicate: (f: Frame) => boolean, timeoutMs = 10_000): Promise<Frame> {
		const existing = this.frames.find(predicate);
		if (existing !== undefined) return Promise.resolve(existing);
		const { promise, resolve, reject } = Promise.withResolvers<Frame>();
		let waiter: FrameWaiter | undefined;
		const timer = setTimeout(() => {
			if (waiter === undefined) return;
			const index = this.waiters.indexOf(waiter);
			if (index === -1) return;
			this.waiters.splice(index, 1);
			reject(new Error(`awaitFrame timed out after ${timeoutMs}ms`));
		}, timeoutMs);
		waiter = {
			predicate,
			resolve: (frame) => {
				clearTimeout(timer);
				resolve(frame);
			},
			timer,
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

function stats(samples: number[]) {
	const sorted = [...samples].sort((a, b) => a - b);
	const mean =
		sorted.length === 0
			? 0
			: sorted.reduce((acc, v) => acc + v, 0) / sorted.length;
	return {
		median: percentile(sorted, 50),
		p95: percentile(sorted, 95),
		p99: percentile(sorted, 99),
		mean,
		n: sorted.length,
	};
}

async function measureTerminalInput(
	stdin: Readable,
	collector: FrameCollector,
	samples: number,
	startId: number,
	data: string,
): Promise<number[]> {
	const latencies: number[] = [];
	let id = startId;
	for (let i = 0; i < samples; i++) {
		const t0 = performance.now();
		stdin.push(
			Buffer.from(
				encodeFrameString({
					id,
					kind: "req",
					method: "terminalInput",
					payload: { data },
				}),
			),
		);
		await collector.awaitFrame((f) => f.id === id && f.kind === "res");
		latencies.push(performance.now() - t0);
		id += 1;
	}
	return latencies;
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

export interface ZeroIdleExtensionMetrics {
	readonly keypressP99: number;
	readonly frameP99: number;
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
	const zeroKey = await measureTerminalInput(zero.stdin, zero.collector, samples, 200, "k");
	const zeroFrame = await measureFrameCpu(zero.stdin, zero.collector, [], samples, 400);
	await shutdown(zero);

	// ----- 100 idle -----
	const idle = await connectHost(Array.from({ length: 100 }, () => idleFactory));
	await waitUntilReady(idle.stdin, idle.collector, 2);
	await measureTerminalInput(idle.stdin, idle.collector, warmups, 100, "k");
	const idleKey = await measureTerminalInput(idle.stdin, idle.collector, samples, 200, "k");
	const idleFrame = await measureFrameCpu(idle.stdin, idle.collector, [], samples, 400);
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

	const zeroKeyStats = stats(zeroKey);
	const idleKeyStats = stats(idleKey);
	const activeKeyStats = stats(activeKey);
	const zeroFrameStats = stats(zeroFrame);
	const idleFrameStats = stats(idleFrame);
	const activeFrameStats = stats(activeFrame);

	failures.push(
		...evaluateZeroIdleSanity(
			{ keypressP99: zeroKeyStats.p99, frameP99: zeroFrameStats.p99 },
			{ keypressP99: idleKeyStats.p99, frameP99: idleFrameStats.p99 },
		),
	);

	// ----- fast terminal input -----
	const fast = await connectHost([terminalInputFastFactory]);
	await waitUntilReady(fast.stdin, fast.collector, 90);
	await sendSessionStart(fast.stdin, fast.collector, 2);
	await measureTerminalInput(fast.stdin, fast.collector, warmups, 100, "a");
	const fastLatencies: number[] = [];
	for (let i = 0; i < samples; i++) {
		const id = 300 + i;
		const data = i % 3 === 0 ? "x" : i % 3 === 1 ? "a" : "b";
		const t0 = performance.now();
		fast.stdin.push(
			Buffer.from(
				encodeFrameString({
					id,
					kind: "req",
					method: "terminalInput",
					payload: { data },
				}),
			),
		);
		await fast.collector.awaitFrame((f) => f.id === id && f.kind === "res");
		fastLatencies.push(performance.now() - t0);
	}
	await shutdown(fast);
	const fastStats = stats(fastLatencies);
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
	if (slowSecondPayload["data"] !== "A") {
		failures.push(
			`later input after disable not local (data=${String(slowSecondPayload["data"])})`,
		);
	}
	if ((slowErr.payload as Record<string, unknown>)["retryable"] !== false) {
		failures.push("extensionError must be non-retryable");
	}

	// ----- mode 2 distinctness: compat vs lean child-process startup -----
	// Same host entry for both modes; only --lean and the fixture differ.
	// Gate is same-run relative (lean p50 / compat p50), hardware-independent.
	const modeDistinctness = await runModeDistinctness({
		hostCwd: EXTENSION_HOST_PACKAGE,
		hostEntry: "src/main.ts",
		compatExtension: resolve(
			EXTENSION_HOST_PACKAGE,
			"fixtures",
			"extensions",
			"idle.ts",
		),
		leanExtension: resolve(
			EXTENSION_HOST_PACKAGE,
			"tests",
			"fixtures",
			"lean",
			"echo.mjs",
		),
		warmups: DEFAULT_WARMUPS,
		samples: DEFAULT_SAMPLES,
		toolRounds: DEFAULT_SAMPLES,
		maxRatio: DEFAULT_LEAN_MAX_RATIO,
	});
	failures.push(...modeDistinctness.failures);

	const artifact = {
		check: 8,
		name: "extension-scaling",
		warmups,
		samples,
		thresholds: {
			idleWithinPctOfZero: 10,
			fastTerminalInputP99Ms: 5,
			slowTerminalInputTimeoutMs: EXTENSION_INPUT_TIMEOUT_MS,
			inputQueueCapacity: EXTENSION_INPUT_QUEUE_CAPACITY,
			// Lean p50 must be <= 85% of compat p50 in the same run: a relative
			// margin far below the observed mode gap, robust to CI noise.
			leanVsCompatP50MaxRatio: DEFAULT_LEAN_MAX_RATIO,
			modeWarmups: DEFAULT_WARMUPS,
			modeSamples: DEFAULT_SAMPLES,
		},
		machine: machineMetadata(),
		results: {
			zero: { keypress: zeroKeyStats, frame: zeroFrameStats },
			idle100: { keypress: idleKeyStats, frame: idleFrameStats },
			active20: {
				keypress: activeKeyStats,
				frame: activeFrameStats,
				widgetKeys: activeKeys.length,
			},
			fastTerminalInput: fastStats,
			slowTerminalInput: {
				firstMs: slowFirstMs,
				secondMs: slowSecondMs,
				activeHandlersAfter: activeAfter,
				firstPayload: slowFirstPayload,
				secondPayload: slowSecondPayload,
			},
			modeDistinctness: {
				compat: modeDistinctness.compat,
				lean: modeDistinctness.lean,
				verdict: modeDistinctness.verdict,
				toolRoundTrip: modeDistinctness.toolRoundTrip,
			},
		},
		pass: failures.length === 0,
		failures,
	};

	const outDir = resolve(process.cwd(), "target", "bench");
	mkdirSync(outDir, { recursive: true });
	const outPath = resolve(outDir, "extension-scaling.json");
	writeFileSync(outPath, `${JSON.stringify(artifact, null, 2)}\n`);

	process.stderr.write(
		`extension-scaling: pass=${artifact.pass} artifact=${outPath}\n` +
			`  zero keypress p99=${zeroKeyStats.p99.toFixed(3)}ms frame p99=${zeroFrameStats.p99.toFixed(3)}ms\n` +
			`  idle100 keypress p99=${idleKeyStats.p99.toFixed(3)}ms frame p99=${idleFrameStats.p99.toFixed(3)}ms\n` +
			`  active20 keypress p99=${activeKeyStats.p99.toFixed(3)}ms frame p99=${activeFrameStats.p99.toFixed(3)}ms\n` +
			`  fast terminalInput p99=${fastStats.p99.toFixed(3)}ms\n` +
			`  slow first=${slowFirstMs.toFixed(3)}ms second=${slowSecondMs.toFixed(3)}ms\n` +
			`  mode2 compat p50=${modeDistinctness.compat.totalMs.median.toFixed(1)}ms ` +
			`lean p50=${modeDistinctness.lean.totalMs.median.toFixed(1)}ms ` +
			`ratio=${modeDistinctness.verdict.ratio.toFixed(3)} (max ${DEFAULT_LEAN_MAX_RATIO})\n` +
			`  lean 3-RPC rounds=${modeDistinctness.toolRoundTrip.rounds} ` +
			`responses=${modeDistinctness.toolRoundTrip.responses} ` +
			`prepare p50=${modeDistinctness.toolRoundTrip.prepareMs.median.toFixed(2)}ms ` +
			`validate p50=${modeDistinctness.toolRoundTrip.validateMs.median.toFixed(2)}ms ` +
			`execute p50=${modeDistinctness.toolRoundTrip.executeMs.median.toFixed(2)}ms\n`,
	);
	if (failures.length > 0) {
		for (const f of failures) process.stderr.write(`  FAIL: ${f}\n`);
		process.exit(1);
	}
}

if (import.meta.main) await main();
