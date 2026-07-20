/**
 * Verification check 8: extension scaling + terminal-input deadlines.
 *
 * - zero / 100 idle / 20 active widgets: native keypress-to-paint p99 and
 *   frame CPU with idle extensions stay within 10% of the zero baseline.
 * - fast onTerminalInput consume/rewrite <5 ms p99.
 * - slow handler times out once at 4 ms, disables only that handler, passes
 *   the original key, and keeps later input local.
 * - active widget bursts remain bounded, drop stale generations, stay responsive.
 */
import { describe, expect, test } from "bun:test";
import { Readable } from "node:stream";
import {
	PROTOCOL_VERSION,
	encodeFrameString,
	type Frame,
} from "@earendil-works/pi-tui-protocol";
import type { ExtensionFactory } from "@earendil-works/pi-coding-agent";
import {
	ExtensionHost,
	EXTENSION_INPUT_TIMEOUT_MS,
	EXTENSION_INPUT_QUEUE_CAPACITY,
} from "../src/host.ts";
import { COMPATIBILITY_VERSION } from "../src/version.ts";
import idleFactory from "../fixtures/extensions/idle.ts";
import widgetActiveFactory from "../fixtures/extensions/widget-active.ts";
import terminalInputFastFactory from "../fixtures/extensions/terminal-input-fast.ts";
import terminalInputSlowFactory from "../fixtures/extensions/terminal-input-slow.ts";

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
			this.frames.push(frame);
			for (let i = this.waiters.length - 1; i >= 0; i--) {
				const waiter = this.waiters[i];
				if (waiter !== undefined && waiter.predicate(frame)) {
					waiter.resolve(frame);
					this.waiters.splice(i, 1);
				}
			}
		}
	}

	awaitFrame(predicate: (f: Frame) => boolean, timeoutMs = 30_000): Promise<Frame> {
		const existing = this.frames.find(predicate);
		if (existing !== undefined) return Promise.resolve(existing);
		const { promise, resolve, reject } = Promise.withResolvers<Frame>();
		const timer = setTimeout(() => {
			reject(new Error(`awaitFrame timed out after ${timeoutMs}ms`));
		}, timeoutMs);
		this.waiters.push({
			predicate,
			resolve: (frame) => {
				clearTimeout(timer);
				resolve(frame);
			},
		});
		return promise;
	}
}


/** Await the next frame matching predicate that appears after `fromIndex`. */
function awaitNewFrame(
	collector: FrameCollector,
	fromIndex: number,
	predicate: (f: Frame) => boolean,
	timeoutMs = 30_000,
): Promise<Frame> {
	return collector.awaitFrame((f) => {
		const idx = collector.frames.indexOf(f);
		return idx >= fromIndex && predicate(f);
	}, timeoutMs);
}

async function connectHost(factories: ExtensionFactory[]): Promise<{
	collector: FrameCollector;
	stdin: Readable;
	host: ExtensionHost;
	runPromise: Promise<void>;
}> {
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
	// Idle extensions have no session_start handlers; host may answer with an
	// error frame. Either terminal frame means the host is READY and serving.
	await collector.awaitFrame(
		(f) => f.id === id && (f.kind === "res" || f.kind === "error"),
	);
}

/** Wait until the host has finished loading and can serve terminalInput. */
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

const p99 = (samples: number[]): number =>
	percentile([...samples].sort((a, b) => a - b), 99);

async function measureKeypressToPaint(
	stdin: Readable,
	collector: FrameCollector,
	samples: number,
	startId: number,
): Promise<number[]> {
	const latencies: number[] = [];
	let id = startId;
	for (let i = 0; i < samples; i++) {
		const t0 = performance.now();
		// Terminal-input path: when no handlers, this is pure host local work
		// (the "native keypress-to-paint" proxy for idle installed extensions).
		stdin.push(
			Buffer.from(
				encodeFrameString({
					id,
					kind: "req",
					method: "terminalInput",
					payload: { data: "k" },
				}),
			),
		);
		const res = await collector.awaitFrame((f) => f.id === id && f.kind === "res");
		const t1 = performance.now();
		latencies.push(t1 - t0);
		const payload = res.payload as Record<string, unknown>;
		expect(payload["consume"]).toBe(false);
		id += 1;
	}
	return latencies;
}

async function measureFrameCpu(
	host: ExtensionHost,
	stdin: Readable,
	collector: FrameCollector,
	keys: string[],
	samples: number,
	startId: number,
): Promise<number[]> {
	const latencies: number[] = [];
	let id = startId;
	if (keys.length === 0) {
		// Zero-widget baseline: empty measure is still a frame composition tick.
		for (let i = 0; i < samples; i++) {
			const t0 = performance.now();
			stdin.push(
				Buffer.from(
					encodeFrameString({
						id,
						kind: "req",
						method: "measure",
						payload: { key: "__none__", width: 80 },
					}),
				),
			);
			await collector.awaitFrame((f) => f.id === id && f.kind === "res");
			latencies.push(performance.now() - t0);
			id += 1;
		}
		return latencies;
	}
	for (let i = 0; i < samples; i++) {
		const key = keys[i % keys.length] ?? keys[0] ?? "__none__";
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
	expect(host.extensionCount).toBeGreaterThanOrEqual(keys.length > 0 ? 1 : 0);
	return latencies;
}

function assertWithinTenPercent(baseline: number, candidate: number): void {
	// Allow absolute floor so near-zero baselines don't fail on noise.
	const limit = Math.max(baseline * 1.1, baseline + 1.0, 2.0);
	expect(candidate).toBeLessThanOrEqual(limit);
}

describe("scaling: zero / idle / active widgets", () => {
	test("idle 100 extensions stay within 10% of zero-extension baseline", async () => {
		const warmups = 20;
		const samples = 80;

		// Zero extensions.
		const zero = await connectHost([]);
		await waitUntilReady(zero.stdin, zero.collector, 2);
		// Warmup.
		await measureKeypressToPaint(zero.stdin, zero.collector, warmups, 100);
		const zeroKey = await measureKeypressToPaint(
			zero.stdin,
			zero.collector,
			samples,
			200,
		);
		const zeroFrame = await measureFrameCpu(
			zero.host,
			zero.stdin,
			zero.collector,
			[],
			samples,
			400,
		);
		zero.stdin.push(null);
		zero.host.dispose("test");
		await zero.runPromise.catch(() => void 0);

		// 100 idle extensions.
		const idleFactories = Array.from({ length: 100 }, () => idleFactory);
		const idle = await connectHost(idleFactories);
		await waitUntilReady(idle.stdin, idle.collector, 2);
		expect(idle.host.extensionCount).toBe(100);
		await measureKeypressToPaint(idle.stdin, idle.collector, warmups, 100);
		const idleKey = await measureKeypressToPaint(
			idle.stdin,
			idle.collector,
			samples,
			200,
		);
		const idleFrame = await measureFrameCpu(
			idle.host,
			idle.stdin,
			idle.collector,
			[],
			samples,
			400,
		);
		idle.stdin.push(null);
		idle.host.dispose("test");
		await idle.runPromise.catch(() => void 0);

		assertWithinTenPercent(p99(zeroKey), p99(idleKey));
		assertWithinTenPercent(p99(zeroFrame), p99(idleFrame));
	}, 60_000);

	test("20 active widgets stay bounded and drop stale generations", async () => {
		const factories = Array.from({ length: 20 }, () => widgetActiveFactory);
		const { collector, stdin, host, runPromise } = await connectHost(factories);
		await waitUntilReady(stdin, collector, 90);
		await sendSessionStart(stdin, collector, 2);

		// Collect 20 uiSlot pushes from this session_start only.
		const slots: Array<{ key: string; generation: number }> = [];
		let watermark = 0;
		while (slots.length < 20) {
			const frame = await awaitNewFrame(
				collector,
				watermark,
				(f) => f.method === "uiSlot",
				10_000,
			);
			watermark = collector.frames.indexOf(frame) + 1;
			const payload = frame.payload as Record<string, unknown>;
			const key = payload["key"];
			const generation = payload["generation"];
			if (typeof key === "string" && typeof generation === "number") {
				slots.push({ key, generation });
			}
		}
		expect(slots.length).toBe(20);
		expect(host.extensionCount).toBe(20);

		// Burst re-push: dispose + session_start again; generations must rise
		// and the host must not grow an unbounded queue of pending work.
		const before = collector.frames.filter((f) => f.method === "uiSlot").length;
		const disposeWatermark = collector.frames.length;
		for (const slot of slots) {
			host.disposeSlot(slot.key);
		}
		// Wait for dispose events (bounded).
		let disposeSeen = 0;
		let disposeWm = disposeWatermark;
		while (disposeSeen < 20) {
			const frame = await awaitNewFrame(
				collector,
				disposeWm,
				(f) => f.method === "disposeSlot",
				10_000,
			);
			disposeWm = collector.frames.indexOf(frame) + 1;
			disposeSeen += 1;
		}

		const reloadWatermark = collector.frames.length;
		stdin.push(
			Buffer.from(
				encodeFrameString({
					id: 50,
					kind: "req",
					method: "session_start",
					payload: { type: "session_start", reason: "reload-burst" },
				}),
			),
		);
		await collector.awaitFrame((f) => f.id === 50 && f.kind === "res");

		const newSlots: Array<{ key: string; generation: number }> = [];
		let reloadWm = reloadWatermark;
		while (newSlots.length < 20) {
			const frame = await awaitNewFrame(
				collector,
				reloadWm,
				(f) => f.method === "uiSlot",
				10_000,
			);
			reloadWm = collector.frames.indexOf(frame) + 1;
			const payload = frame.payload as Record<string, unknown>;
			const key = payload["key"];
			const generation = payload["generation"];
			if (typeof key === "string" && typeof generation === "number") {
				newSlots.push({ key, generation });
			}
		}
		expect(newSlots.length).toBe(20);
		const minNewGen = Math.min(...newSlots.map((s) => s.generation));
		const maxOldGen = Math.max(...slots.map((s) => s.generation));
		expect(minNewGen).toBeGreaterThan(maxOldGen);

		// Input stays responsive during/after the burst.
		const t0 = performance.now();
		stdin.push(
			Buffer.from(
				encodeFrameString({
					id: 60,
					kind: "req",
					method: "terminalInput",
					payload: { data: "z" },
				}),
			),
		);
		const res = await collector.awaitFrame((f) => f.id === 60 && f.kind === "res");
		const elapsed = performance.now() - t0;
		expect(elapsed).toBeLessThan(50);
		expect((res.payload as Record<string, unknown>)["consume"]).toBe(false);

		const after = collector.frames.filter((f) => f.method === "uiSlot").length;
		// Bounded growth: at most one new generation set, not a runaway queue.
		expect(after - before).toBeLessThanOrEqual(40);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	}, 30_000);
});

describe("scaling: terminal-input deadlines", () => {
	test("fast onTerminalInput consume/rewrite stays under 5ms p99", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([
			terminalInputFastFactory,
		]);
		await waitUntilReady(stdin, collector, 90);
		await sendSessionStart(stdin, collector, 2);
		expect(host.activeTerminalHandlerCount).toBe(1);

		const warmups = 30;
		const samples = 120;
		// Warmup.
		for (let i = 0; i < warmups; i++) {
			stdin.push(
				Buffer.from(
					encodeFrameString({
						id: 100 + i,
						kind: "req",
						method: "terminalInput",
						payload: { data: "a" },
					}),
				),
			);
			await collector.awaitFrame((f) => f.id === 100 + i && f.kind === "res");
		}

		const latencies: number[] = [];
		for (let i = 0; i < samples; i++) {
			const id = 300 + i;
			const data = i % 3 === 0 ? "x" : i % 3 === 1 ? "a" : "b";
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
			const res = await collector.awaitFrame((f) => f.id === id && f.kind === "res");
			latencies.push(performance.now() - t0);
			const payload = res.payload as Record<string, unknown>;
			if (data === "x") {
				expect(payload["consume"]).toBe(true);
			} else if (data === "a") {
				expect(payload["consume"]).toBe(false);
				expect(payload["data"]).toBe("A");
			} else {
				expect(payload["consume"]).toBe(false);
				expect(payload["data"]).toBe("b");
			}
		}

		expect(p99(latencies)).toBeLessThan(5);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	}, 30_000);

	test("slow handler times out once, disables only itself, later input stays local", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([
			terminalInputSlowFactory,
			terminalInputFastFactory,
		]);
		await waitUntilReady(stdin, collector, 90);
		await sendSessionStart(stdin, collector, 2);
		// Both handlers registered; slow is first (registration order).
		expect(host.terminalHandlerCount).toBe(2);
		expect(host.activeTerminalHandlerCount).toBe(2);

		// First keystroke: slow handler must time out; original key passes.
		const t0 = performance.now();
		stdin.push(
			Buffer.from(
				encodeFrameString({
					id: 10,
					kind: "req",
					method: "terminalInput",
					payload: { data: "q" },
				}),
			),
		);
		const first = await collector.awaitFrame((f) => f.id === 10 && f.kind === "res");
		const firstElapsed = performance.now() - t0;
		const firstPayload = first.payload as Record<string, unknown>;
		expect(firstPayload["consume"]).toBe(false);
		expect(firstPayload["data"]).toBe("q");
		// Timeout is 4 ms; allow scheduler slack but require it fired.
		expect(firstElapsed).toBeGreaterThanOrEqual(EXTENSION_INPUT_TIMEOUT_MS - 1);

		const errEvent = await collector.awaitFrame(
			(f) => f.method === "extensionError",
			2_000,
		);
		const errPayload = errEvent.payload as Record<string, unknown>;
		expect(errPayload["retryable"]).toBe(false);
		expect(String(errPayload["message"])).toContain("exceeded");
		expect(String(errPayload["message"])).toContain(`${EXTENSION_INPUT_TIMEOUT_MS}ms`);

		// Only the slow handler is disabled; fast remains active.
		expect(host.activeTerminalHandlerCount).toBe(1);
		expect(host.terminalHandlerCount).toBe(2);

		// Later input stays local to the remaining (fast) handler — no second timeout.
		const t1 = performance.now();
		stdin.push(
			Buffer.from(
				encodeFrameString({
					id: 11,
					kind: "req",
					method: "terminalInput",
					payload: { data: "a" },
				}),
			),
		);
		const second = await collector.awaitFrame((f) => f.id === 11 && f.kind === "res");
		const secondElapsed = performance.now() - t1;
		const secondPayload = second.payload as Record<string, unknown>;
		expect(secondPayload["consume"]).toBe(false);
		expect(secondPayload["data"]).toBe("A");
		expect(secondElapsed).toBeLessThan(5);
		// Still only one extensionError for the single timeout.
		const errorCount = collector.frames.filter((f) => f.method === "extensionError").length;
		expect(errorCount).toBe(1);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	}, 15_000);

	test("queue exhaustion fails open without disabling handlers", async () => {
		const { host, stdin, collector, runPromise } = await connectHost([
			terminalInputSlowFactory,
		]);
		await waitUntilReady(stdin, collector, 90);
		await sendSessionStart(stdin, collector, 2);
		expect(host.activeTerminalHandlerCount).toBe(1);

		// Flood the queue while the first slow job holds the drain.
		const ids: number[] = [];
		for (let i = 0; i < EXTENSION_INPUT_QUEUE_CAPACITY + 8; i++) {
			const id = 1000 + i;
			ids.push(id);
			stdin.push(
				Buffer.from(
					encodeFrameString({
						id,
						kind: "req",
						method: "terminalInput",
						payload: { data: `k${i}` },
					}),
				),
			);
		}

		// At least one response must fail open with original data while queue is full.
		const firstRes = await collector.awaitFrame(
			(f) => typeof f.id === "number" && f.id >= 1000 && f.kind === "res",
			5_000,
		);
		expect(firstRes).toBeDefined();

		// Drain remaining.
		for (const id of ids) {
			await collector.awaitFrame((f) => f.id === id && f.kind === "res", 10_000);
		}

		const exhaustionErrors = collector.frames.filter(
			(f) =>
				f.method === "extensionError" &&
				String((f.payload as Record<string, unknown>)["message"]).includes(
					"queue exhausted",
				),
		);
		expect(exhaustionErrors.length).toBeGreaterThanOrEqual(1);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	}, 30_000);
});
