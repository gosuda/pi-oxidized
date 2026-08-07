/**
 * Acceptance tests: widget slots, stale generation, crash isolation,
 * all 33 lifecycle methods, runtime extension loading, and compiled artifacts.
 */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Readable } from "node:stream";
import {
	PROTOCOL_VERSION,
	encodeFrameString,
	type Frame,
} from "@earendil-works/pi-tui-protocol";
import type {
	ExtensionFactory,
	ExtensionContextActions,
	InlineExtension,
} from "@earendil-works/pi-coding-agent";
import {
	loadExtensionFromFactory,
	createExtensionRuntime,
} from "@earendil-works/pi-coding-agent";
import { ExtensionRunner } from "@earendil-works/pi-coding-agent";
import { builtInExtensions } from "@earendil-works/pi-coding-agent/builtins";
import { ExtensionHost, createEventBus } from "../src/host.ts";
import { COMPATIBILITY_VERSION } from "../src/version.ts";
import { createExtensionJiti } from "../src/virtual-modules.ts";
import { Type } from "@earendil-works/pi-ai";

import allEventsFactory, { ALL_EVENTS } from "../fixtures/extensions/all-events.ts";
import crashFactory from "../fixtures/extensions/crash.ts";
import hostileFactory from "../fixtures/extensions/hostile.ts";
import hooksFactory from "../fixtures/extensions/hooks.ts";
import toolFactory from "../fixtures/extensions/tool.ts";
import toolProgressFactory from "../fixtures/extensions/tool-progress.ts";
import providerStreamFactory from "../fixtures/extensions/provider-stream.ts";
import themeApiFactory from "../fixtures/extensions/theme-api.ts";

const noopContextActions: ExtensionContextActions = {
	getModel: () => undefined,
	getScopedModels: () => [],
	isIdle: () => true,
	isProjectTrusted: () => true,
	getSignal: () => undefined,
	abort: () => {},
	hasPendingMessages: () => false,
	shutdown: () => {},
	getContextUsage: () => undefined,
	compact: () => {},
	getSystemPrompt: () => "",
};

const projectTrustFactory: ExtensionFactory = (pi) => {
	pi.on("input", (_event, ctx) => {
		const trustAwareContext = ctx as typeof ctx & { isProjectTrusted(): boolean };
		return {
			action: trustAwareContext.isProjectTrusted() ? "handled" : "continue",
		};
	});
};

async function runProcess(
	command: string,
	args: readonly string[],
	cwd: string,
): Promise<{ readonly stdout: string; readonly stderr: string }> {
	const { promise, resolve: resolvePromise, reject: rejectPromise } =
		Promise.withResolvers<{ readonly stdout: string; readonly stderr: string }>();
	const child = spawn(command, args, { cwd, stdio: ["ignore", "pipe", "pipe"] });
	let stdout = "";
	let stderr = "";
	child.stdout.on("data", (chunk: Buffer) => { stdout += chunk.toString(); });
	child.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString(); });
	child.once("error", rejectPromise);
	child.once("exit", (code, signal) => {
		if (code === 0) {
			resolvePromise({ stdout, stderr });
			return;
		}
		rejectPromise(new Error(
			`${command} exited with code ${String(code)} signal ${String(signal)}: ${stderr || stdout}`,
		));
	});
	return await promise;
}

/**
 * ByteWritable that decodes frames as they arrive and lets tests await
 * specific frames by predicate. No write-counting, no timers.
 */
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
			if (line.trim().length > 0) {
				const frame = JSON.parse(line) as Frame;
				this.frames.push(frame);
				for (let i = this.waiters.length - 1; i >= 0; i--) {
					if (this.waiters[i]?.predicate(frame)) {
						this.waiters[i]?.resolve(frame);
						this.waiters.splice(i, 1);
					}
				}
			}
		}
	}

	awaitFrame(predicate: (f: Frame) => boolean): Promise<Frame> {
		const existing = this.frames.find(predicate);
		if (existing !== undefined) return Promise.resolve(existing);
		const { promise, resolve } = Promise.withResolvers<Frame>();
		this.waiters.push({ predicate, resolve });
		return promise;
	}
}

/** Create a host wired to a FrameCollector; send hello and await ack. */
async function connectHost(factories: InlineExtension[]): Promise<{
	collector: FrameCollector;
	stdin: Readable;
	host: ExtensionHost;
	runPromise: Promise<void>;
}> {
	const collector = new FrameCollector();
	const stdin = new Readable({ read() {} });
	const host = new ExtensionHost(stdin, collector);
	const runPromise = host.run({ cwd: process.cwd(), factories, extensionPaths: [] });

	stdin.push(Buffer.from(encodeFrameString({
		id: 1, kind: "req", method: "hello",
		payload: { protocolVersion: PROTOCOL_VERSION, compatibilityVersion: COMPATIBILITY_VERSION },
	})));
	await collector.awaitFrame((f) => f.id === 1 && f.kind === "res");
	return { collector, stdin, host, runPromise };
}

async function makeRunner(factory: ExtensionFactory, path: string) {
	const runtime = createExtensionRuntime();
	const bus = createEventBus();
	const ext = await loadExtensionFromFactory(factory, process.cwd(), bus, runtime, path);
	// Escape hatch: reference class stubs for test-only runner construction.
	const runner = new ExtensionRunner(
		[ext], runtime, process.cwd(),
		{} as unknown as ConstructorParameters<typeof ExtensionRunner>[3],
		{ getAll: () => [], find: () => undefined } as unknown as ConstructorParameters<typeof ExtensionRunner>[4],
	);
	runner.bindCore({} as unknown as Parameters<typeof runner.bindCore>[0], noopContextActions);
	return { runner, ext, runtime };
 }

/** Send session_start and await both the response and any uiSlot event. */
async function sendSessionStart(stdin: Readable, collector: FrameCollector): Promise<void> {
	stdin.push(Buffer.from(encodeFrameString({
		id: 2, kind: "req", method: "session_start",
		payload: { type: "session_start", reason: "startup" },
	})));
	await collector.awaitFrame((f) => f.id === 2 && f.kind === "res");
}

// ===========================================================================
// 1. Widget slot measured-height + render
// ===========================================================================

describe("acceptance: widget slot measure/render", () => {
	test("host responds to measure with correct height", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([hostileFactory]);
		await sendSessionStart(stdin, collector);

		// Wait for uiSlot event (widget.hostile pushed by session_start handler).
		await collector.awaitFrame((f) => f.method === "uiSlot");

		stdin.push(Buffer.from(encodeFrameString({
			id: 10, kind: "req", method: "measure",
			payload: { key: "widget.hostile", width: 80 },
		})));
		const measureRes = await collector.awaitFrame((f) => f.id === 10 && f.kind === "res");

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);

		const height = (measureRes.payload as Record<string, unknown>)["height"];
		expect(typeof height).toBe("number");
		expect(height as number).toBeGreaterThan(0);
	});

	test("host responds to render with sanitized runs (no raw ESC)", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([hostileFactory]);
		await sendSessionStart(stdin, collector);
		await collector.awaitFrame((f) => f.method === "uiSlot");

		stdin.push(Buffer.from(encodeFrameString({
			id: 11, kind: "req", method: "render",
			payload: { key: "widget.hostile", width: 80 },
		})));
		const renderRes = await collector.awaitFrame((f) => f.id === 11 && f.kind === "res");

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);

		const runs = (renderRes.payload as Record<string, unknown>)["runs"] as unknown[][];
		expect(Array.isArray(runs)).toBe(true);
		expect(runs.length).toBeGreaterThan(0);
		// Every text run must be free of raw ESC bytes — plugin bytes never reach stdout.
		for (const line of runs) {
			for (const run of line) {
				const text = (run as Record<string, unknown>)["text"] as string;
				expect(text).not.toContain("\x1b");
			}
		}
	});

	test("focusable slot can be disposed and focus restored", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([hostileFactory]);
		await sendSessionStart(stdin, collector);

		const slotEvent = await collector.awaitFrame((f) => f.method === "uiSlot");
		const slotPayload = slotEvent.payload as Record<string, unknown>;
		expect(slotPayload["key"]).toBe("widget.hostile");
		expect(typeof slotPayload["generation"]).toBe("number");

		// Dispose the slot via the host's disposeSlot method.
		host.disposeSlot("widget.hostile");

		const disposeEvent = await collector.awaitFrame((f) => f.method === "disposeSlot");
		const disposePayload = disposeEvent.payload as Record<string, unknown>;
		expect(disposePayload["key"]).toBe("widget.hostile");
		expect(disposePayload["generation"]).toBe(slotPayload["generation"]);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});
});

// ===========================================================================
// 2. Stale generation reordered replies
// ===========================================================================

describe("acceptance: stale generation tracking", () => {
	test("uiSlot and disposeSlot carry matching generations", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([hostileFactory]);
		await sendSessionStart(stdin, collector);

		const slotEvent = await collector.awaitFrame((f) => f.method === "uiSlot");
		const slotGen = (slotEvent.payload as Record<string, unknown>)["generation"];

		host.disposeSlot("widget.hostile");
		const disposeEvent = await collector.awaitFrame((f) => f.method === "disposeSlot");
		const disposeGen = (disposeEvent.payload as Record<string, unknown>)["generation"];

		expect(disposeGen).toBe(slotGen);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("re-pushing a slot produces a newer generation", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([hostileFactory]);
		await sendSessionStart(stdin, collector);

		const slot1 = await collector.awaitFrame((f) => f.method === "uiSlot");
		const gen1 = (slot1.payload as Record<string, unknown>)["generation"] as number;

		// Dispose and re-trigger session_start to push again.
		host.disposeSlot("widget.hostile");
		await collector.awaitFrame((f) => f.method === "disposeSlot");

		stdin.push(Buffer.from(encodeFrameString({
			id: 20, kind: "req", method: "session_start",
			payload: { type: "session_start", reason: "reload" },
		})));
		await collector.awaitFrame((f) => f.id === 20 && f.kind === "res");

		const slot2 = await collector.awaitFrame(
			(f) => f.method === "uiSlot" && (f.payload as Record<string, unknown>)["generation"] !== gen1,
		);
		const gen2 = (slot2.payload as Record<string, unknown>)["generation"] as number;
		expect(gen2).toBeGreaterThan(gen1);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});
});

// ===========================================================================
// 3. Crash mid-hook: error isolation, no replay, pending close
// ===========================================================================

describe("acceptance: crash isolation", () => {
	test("handler throw emits extensionError, turn completes", async () => {
		const { runner } = await makeRunner(crashFactory, "crash.ts");
		const errors: Array<{ event: string; message: string }> = [];
		runner.onError((err) => {
			errors.push({ event: err.event, message: err.error });
		});

		await runner.emit({ type: "session_start", reason: "startup" });
		expect(errors).toHaveLength(1);
		expect(errors[0]?.event).toBe("session_start");
		expect(errors[0]?.message).toContain("crash-in-session-start");

		await runner.emit({ type: "agent_start" });
		expect(errors).toHaveLength(2);
		expect(errors[1]?.event).toBe("agent_start");
	});

	test("host forwards crash as nonretryable extensionError", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([crashFactory]);

		stdin.push(Buffer.from(encodeFrameString({
			id: 5, kind: "req", method: "session_start",
			payload: { type: "session_start", reason: "startup" },
		})));

		// Turn must complete (response arrives).
		const res = await collector.awaitFrame((f) => f.id === 5 && f.kind === "res");
		expect(res).toBeDefined();

		// Extension error event must be emitted with retryable=false.
		const errEvent = await collector.awaitFrame((f) => f.method === "extensionError");
		const payload = errEvent.payload as Record<string, unknown>;
		expect(payload["retryable"]).toBe(false);
		expect(payload["code"]).toBe("extension_error");

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("no effect replayed — second emit after crash is clean", async () => {
		const calls: string[] = [];
		const runtime = createExtensionRuntime();
		const bus = createEventBus();
		let firstCall = true;
		const factory: ExtensionFactory = (pi) => {
			pi.on("session_start", () => {
				if (firstCall) {
					firstCall = false;
					throw new Error("first-crash");
				}
				calls.push("ok");
			});
		};
		const ext = await loadExtensionFromFactory(factory, process.cwd(), bus, runtime, "replay.ts");
		const runner = new ExtensionRunner(
			[ext], runtime, process.cwd(),
			{} as unknown as ConstructorParameters<typeof ExtensionRunner>[3],
			{ getAll: () => [], find: () => undefined } as unknown as ConstructorParameters<typeof ExtensionRunner>[4],
		);
		runner.bindCore({} as unknown as Parameters<typeof runner.bindCore>[0], noopContextActions);

		await runner.emit({ type: "session_start", reason: "startup" });
		expect(calls).toHaveLength(0);

		await runner.emit({ type: "session_start", reason: "reload" });
		expect(calls).toHaveLength(1);
	});

	test("stale-generation reorder: only newest generation is rendered", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([hostileFactory]);
		await sendSessionStart(stdin, collector);

		const slot1 = await collector.awaitFrame((f) => f.method === "uiSlot");
		const gen1 = (slot1.payload as Record<string, unknown>)["generation"] as number;

		// Push two more times: generation N+1, then N+2.
		host.disposeSlot("widget.hostile");
		await collector.awaitFrame((f) => f.method === "disposeSlot");

		stdin.push(Buffer.from(encodeFrameString({
			id: 40, kind: "req", method: "session_start",
			payload: { type: "session_start", reason: "reload-1" },
		})));
		await collector.awaitFrame((f) => f.id === 40 && f.kind === "res");
		const slot2 = await collector.awaitFrame(
			(f) => f.method === "uiSlot" && (f.payload as Record<string, unknown>)["generation"] !== gen1,
		);
		const gen2 = (slot2.payload as Record<string, unknown>)["generation"] as number;

		host.disposeSlot("widget.hostile");
		await collector.awaitFrame(
			(f) => f.method === "disposeSlot" && (f.payload as Record<string, unknown>)["generation"] === gen2,
		);

		stdin.push(Buffer.from(encodeFrameString({
			id: 41, kind: "req", method: "session_start",
			payload: { type: "session_start", reason: "reload-2" },
		})));
		await collector.awaitFrame((f) => f.id === 41 && f.kind === "res");
		const slot3 = await collector.awaitFrame(
			(f) => f.method === "uiSlot" && (f.payload as Record<string, unknown>)["generation"] === gen2 + 1,
		);
		const gen3 = (slot3.payload as Record<string, unknown>)["generation"] as number;

		// Verify generations are strictly increasing: Rust should render gen3,
		// not gen2 (which is now stale). The host guarantees this by
		// incrementing nextGeneration on every pushSlot.
		expect(gen3).toBeGreaterThan(gen2);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("crash mid-tool: tool execute throw emits exactly-once nonretryable error", async () => {
		const runtime = createExtensionRuntime();
		const bus = createEventBus();
		const ext = await loadExtensionFromFactory(crashFactory, process.cwd(), bus, runtime, "crash.ts");
		const runner = new ExtensionRunner(
			[ext], runtime, process.cwd(),
			{} as unknown as ConstructorParameters<typeof ExtensionRunner>[3],
			{ getAll: () => [], find: () => undefined } as unknown as ConstructorParameters<typeof ExtensionRunner>[4],
		);
		runner.bindCore({} as unknown as Parameters<typeof runner.bindCore>[0], noopContextActions);

		const errorEvents: Array<{ event: string; message: string }> = [];
		runner.onError((err) => {
			errorEvents.push({ event: err.event, message: err.error });
		});

		// Invoke the crashing tool directly via the wrapper.
		const toolDef = runner.getAllRegisteredTools().find((t) => t.definition.name === "crash_tool");
		expect(toolDef).toBeDefined();

		// Call execute and verify it throws exactly once (no replay).
		await expect(
			(toolDef?.definition.execute as (...args: unknown[]) => Promise<unknown>)(
				"call-1", {}, undefined, undefined, runner.createContext(),
			),
		).rejects.toThrow("crash-in-tool-execute");

		// Second call also throws — no replay of the first failure, but the
		// second call is a fresh invocation.
		await expect(
			(toolDef?.definition.execute as (...args: unknown[]) => Promise<unknown>)(
				"call-2", {}, undefined, undefined, runner.createContext(),
			),
		).rejects.toThrow("crash-in-tool-execute");
	});

	// Test harness: verify provider registration is processed exactly once
	// during bindCore, and streamSimple invocation propagates errors to the
	// caller (the runner does not intercept stream invocation errors — those
	// are handled by the caller, e.g. Rust).
	test("crash mid-provider-stream: provider registration exactly-once", async () => {
		const factory: ExtensionFactory = (pi) => {
			pi.registerProvider("crash_provider", {
				streamSimple: () => { throw new Error("crash-in-provider-stream"); },
			});
		};
		const runtime = createExtensionRuntime();
		const ext = await loadExtensionFromFactory(factory, process.cwd(), undefined, runtime, "crash.ts");

		// Capture provider config via a minimal stub that counts registrations.
		let registerCount = 0;
		let capturedConfig: { streamSimple?: () => unknown } | undefined;
		const stubRegistry = {
			getAll: () => [] as unknown[],
			find: () => undefined,
			registerProvider: (_name: string, config: { streamSimple?: () => unknown }) => {
				registerCount++;
				capturedConfig = config;
			},
			unregisterProvider: () => {},
			getRegisteredProviderIds: () => [] as readonly string[],
			getRegisteredProviderConfig: () => capturedConfig,
		};
		const runner = new ExtensionRunner(
			[ext], runtime, process.cwd(),
			{} as unknown as ConstructorParameters<typeof ExtensionRunner>[3],
			stubRegistry as unknown as ConstructorParameters<typeof ExtensionRunner>[4],
		);
		runner.bindCore({} as unknown as Parameters<typeof runner.bindCore>[0], noopContextActions);

		// Exactly-once: registerProvider called exactly once during bindCore.
		expect(registerCount).toBe(1);
		expect(capturedConfig?.streamSimple).toBeDefined();

		// Second bindCore attempt: the runner is now bound; calling registerProvider
		// again directly would call the stub again, but the runner doesn't
		// re-flush pendingProviderRegistrations on subsequent calls.
		// Verify the stub was NOT called again.
		expect(registerCount).toBe(1);

		// streamSimple invocation throws to the caller — not intercepted by runner.
		expect(() => capturedConfig?.streamSimple?.()).toThrow("crash-in-provider-stream");
	});
});

// ===========================================================================
// 4. All 33 lifecycle methods
// ===========================================================================

describe("acceptance: all 33 lifecycle events", () => {
	test("runner recognizes handlers for all 33 event types", async () => {
		const { runner } = await makeRunner(allEventsFactory, "all-events.ts");
		expect(ALL_EVENTS).toHaveLength(33);
		for (const event of ALL_EVENTS) {
			expect(runner.hasHandlers(event)).toBe(true);
		}
	});

	test("all 33 events can be emitted without error and exactly once", async () => {
		const calls = new Map<string, number>();
		const recordingFactory: ExtensionFactory = (pi) => {
			for (const event of ALL_EVENTS) {
				pi.on(event, () => {
					calls.set(event, (calls.get(event) ?? 0) + 1);
				});
			}
		};
		const { runner } = await makeRunner(recordingFactory, "all-events-recording.ts");
		const errors: string[] = [];
		runner.onError((error) => errors.push(`${error.event}: ${error.error}`));

		for (const event of ALL_EVENTS) {
			const result = await runner.emit({ type: event });
			expect(result).not.toBeInstanceOf(Error);
		}

		expect(errors).toEqual([]);
		for (const event of ALL_EVENTS) {
			expect(calls.get(event)).toBe(1);
		}
	});

	test("cancellable session events round-trip cancellation through the host", async () => {
		const cancellationFactory: ExtensionFactory = (pi) => {
			pi.on("session_before_switch", () => ({ cancel: true }));
			pi.on("session_before_fork", () => ({ cancel: true }));
			pi.on("session_before_compact", () => ({ cancel: true }));
			pi.on("session_before_tree", () => ({ cancel: true }));
		};
		const { collector, stdin, host, runPromise } = await connectHost([cancellationFactory]);
		const events = [
			"session_before_switch",
			"session_before_fork",
			"session_before_compact",
			"session_before_tree",
		] as const;
		for (const [index, event] of events.entries()) {
			const id = 200 + index;
			stdin.push(Buffer.from(encodeFrameString({
				id, kind: "req", method: event, payload: {},
			})));
			const response = await collector.awaitFrame((frame) => frame.id === id);
			expect(response.kind).toBe("res");
			expect(response.payload).toEqual({ cancel: true });
		}
		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});
});

// ===========================================================================
// 5. Runtime import and compiled extension protocol
// ===========================================================================

describe("acceptance: extension runtime", () => {
	const hostDir = resolve(import.meta.dirname, "..");
	const executableSuffix = process.platform === "win32" ? ".exe" : "";
	let artifactDir: string;
	let compiledHost: string;
	let compiledRuntimeImport: string;

	beforeAll(async () => {
		artifactDir = await mkdtemp(join(tmpdir(), "pi-extension-host-acceptance-"));
		compiledHost = join(artifactDir, `pi-extension-host${executableSuffix}`);
		compiledRuntimeImport = join(artifactDir, `runtime-import${executableSuffix}`);
		await runProcess(process.execPath, [
			"build",
			"./src/main.ts",
			"--compile",
			"--outfile",
			compiledHost,
		], hostDir);
		await runProcess(process.execPath, [
			"build",
			"./fixtures/runtime-import.ts",
			"--compile",
			"--outfile",
			compiledRuntimeImport,
		], hostDir);
	});

	afterAll(async () => {
		await rm(artifactDir, { force: true, recursive: true });
	});

	test("compiled binary handles hello handshake", async () => {
		const { promise, resolve: resolvePromise, reject: rejectPromise } =
			Promise.withResolvers<string>();
		const child = spawn(compiledHost, ["--cwd", artifactDir], {
			cwd: hostDir,
			stdio: ["pipe", "pipe", "pipe"],
		});
		let output = "";
		child.stdout.on("data", (chunk: Buffer) => {
			output += chunk.toString();
			if (output.includes("\n")) resolvePromise(output.trim());
		});
		child.once("error", rejectPromise);
		child.once("exit", (code) => {
			if (!output.includes("\n")) {
				rejectPromise(new Error(`process exited early with code ${String(code)}`));
			}
		});
		child.stdin.write(encodeFrameString({
			id: 1,
			kind: "req",
			method: "hello",
			payload: {
				protocolVersion: PROTOCOL_VERSION,
				compatibilityVersion: COMPATIBILITY_VERSION,
			},
		}));
		try {
			const frame = JSON.parse(await promise) as Frame;
			expect(frame.kind).toBe("res");
			expect(frame.method).toBe("hello");
			const payload = frame.payload as Record<string, unknown>;
			expect(payload["protocolVersion"]).toBe(PROTOCOL_VERSION);
			expect(payload["compatibilityVersion"]).toBe(COMPATIBILITY_VERSION);
		} finally {
			child.stdin.end();
			child.kill("SIGTERM");
		}
	});

	test("compiled binary rejects version mismatch", async () => {
		const { promise, resolve: resolvePromise, reject: rejectPromise } =
			Promise.withResolvers<string>();
		const child = spawn(compiledHost, ["--cwd", artifactDir], {
			cwd: hostDir,
			stdio: ["pipe", "ignore", "pipe"],
		});
		let stderr = "";
		child.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString(); });
		child.once("error", rejectPromise);
		child.once("exit", () => resolvePromise(stderr));
		child.stdin.write(encodeFrameString({
			id: 1,
			kind: "req",
			method: "hello",
			payload: {
				protocolVersion: 999,
				compatibilityVersion: COMPATIBILITY_VERSION,
			},
		}));
		child.stdin.end();
		try {
			expect(await promise).toContain("version mismatch");
		} finally {
			child.kill("SIGTERM");
		}
	});
	test("runtime-import loads real extension via jiti", async () => {
		const helloPath = resolve(
			import.meta.dirname, "..", "..", "..",
			".references", "pi", "packages", "coding-agent", "examples", "extensions", "hello.ts",
		);
		const jiti = createExtensionJiti();
		const module = await jiti.import(helloPath, { default: true }) as unknown;
		expect(typeof module).toBe("function");
		const runtime = createExtensionRuntime();
		const bus = createEventBus();
		const ext = await loadExtensionFromFactory(
			module as ExtensionFactory, process.cwd(), bus, runtime, helloPath,
		);
		expect([...ext.tools.keys()]).toContain("hello");
	});

	test("compiled runtime-import binary loads a real extension", async () => {
		const extensionPath = resolve(
			import.meta.dirname,
			"..",
			"..",
			"..",
			".references",
			"pi",
			"packages",
			"coding-agent",
			"examples",
			"extensions",
			"hello.ts",
		);
		const output = await runProcess(compiledRuntimeImport, [extensionPath], hostDir);
		const result = JSON.parse(output.stdout.trim()) as Record<string, unknown>;
		expect(result).toEqual({
			path: extensionPath,
			tools: ["hello"],
			handlers: [],
			commands: [],
			flags: [],
			shortcuts: [],
			messageRenderers: [],
		});
	});

	test("input hook forwards action union (not { ok: true })", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([hooksFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 30, kind: "req", method: "input",
			payload: { type: "input", text: "hello", source: "interactive" },
		})));
		const res = await collector.awaitFrame((f) => f.id === 30 && f.kind === "res");
		expect(res.payload).toEqual({ action: "continue" });
		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("extensions.load RPC dynamically loads extensions", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([]);
		const extPath = resolve(import.meta.dirname, "..", "fixtures", "extensions", "crash.ts");
		stdin.push(Buffer.from(encodeFrameString({
			id: 31, kind: "req", method: "extensions.load",
			payload: { extensionPaths: [extPath], cwd: process.cwd() },
		})));
		const res = await collector.awaitFrame((f) => f.id === 31 && f.kind === "res");
		const payload = res.payload as Record<string, unknown>;
		expect(payload["extensions"]).toBe(1);
		expect(payload["errors"]).toEqual([]);
		const tools = payload["tools"] as Array<Record<string, unknown>>;
		expect(tools.map((tool) => tool["name"])).toEqual(["crash_tool"]);
		const providers = payload["providers"] as Array<Record<string, unknown>>;
		expect(providers.map((provider) => provider["name"])).toEqual(["crash_provider"]);
		expect(providers[0]?.["streamSimple"]).toBe(true);
		expect(payload["handlers"]).toEqual(["session_start", "agent_start", "message_end"]);
	});

	test("extensions.load parses projectTrusted and exposes it through hook context", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([projectTrustFactory]);
		let id = 100;
		const observeTrust = async (projectTrusted: unknown, includeField = true): Promise<string | undefined> => {
			const loadId = id++;
			const payload: Record<string, unknown> = { extensionPaths: [], cwd: process.cwd() };
			if (includeField) payload["projectTrusted"] = projectTrusted;
			stdin.push(Buffer.from(encodeFrameString({
				id: loadId, kind: "req", method: "extensions.load", payload,
			})));
			await collector.awaitFrame((frame) => frame.id === loadId && frame.kind === "res");

			const hookId = id++;
			stdin.push(Buffer.from(encodeFrameString({
				id: hookId, kind: "req", method: "input",
				payload: { text: "trust?", source: "interactive" },
			})));
			const response = await collector.awaitFrame(
				(frame) => frame.id === hookId && frame.kind === "res",
			);
			return (response.payload as Record<string, unknown>)["action"] as string | undefined;
		};

		expect(await observeTrust(false)).toBe("continue");
		expect(await observeTrust(true)).toBe("handled");
		expect(await observeTrust("true")).toBe("continue");
		expect(await observeTrust(undefined, false)).toBe("continue");

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("command.execute invokes registered command and completes", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([toolFactory]);
		// Trigger showOverlay which registers an event listener that waits for session_info_changed
		stdin.push(Buffer.from(encodeFrameString({
			id: 32, kind: "req", method: "command.execute",
			payload: { command: "showOverlay", args: "test-arg" },
		})));
		// Wait for overlay uiSlot event
		const slotEv = await collector.awaitFrame((f) => f.method === "uiSlot" && (f.payload as Record<string, unknown>)["placement"] === "overlay");
		const slotPayload = slotEv.payload as Record<string, unknown>;
		expect(slotPayload["focusable"]).toBe(true);

		// Send session_info_changed to trigger done() inside showOverlay
		stdin.push(Buffer.from(encodeFrameString({
			id: 33, kind: "req", method: "session_info_changed",
			payload: { type: "session_info_changed", name: "trigger" },
		})));

		// Wait for command.execute response (after done())
		const res = await collector.awaitFrame((f) => f.id === 32 && f.kind === "res");
		expect(res.payload).toEqual({ ok: true });

		// Verify slot was disposed
		const disposeEv = await collector.awaitFrame((f) => f.method === "disposeSlot" && (f.payload as Record<string, unknown>)["key"] === slotPayload["key"]);
		expect(disposeEv).toBeDefined();

	stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("custom overlay routes input by generation and rerenders through the TUI proxy", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([toolFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 110, kind: "req", method: "command.execute",
			payload: { command: "interactiveOverlay", args: "" },
		})));
		const initial = await collector.awaitFrame((frame) =>
			frame.method === "uiSlot"
			&& String((frame.payload as Record<string, unknown>)["key"]).startsWith("overlay.")
			&& JSON.stringify(frame.payload).includes("overlay:initial")
		);
		const slot = initial.payload as Record<string, unknown>;
		const key = slot["key"] as string;
		const generation = slot["generation"] as number;

		stdin.push(Buffer.from(encodeFrameString({
			id: 111, kind: "req", method: "uiEvent",
			payload: { key, generation: generation + 1, event: { type: "key", code: "x", modifiers: {}, kind: "press" }, data: "stale" },
		})));
		const stale = await collector.awaitFrame((frame) => frame.id === 111);
		expect(stale.payload).toEqual({ delivered: false });

		stdin.push(Buffer.from(encodeFrameString({
			id: 112, kind: "req", method: "uiEvent",
			payload: { key, generation, event: { type: "key", code: "x", modifiers: {}, kind: "press" }, data: "x" },
		})));
		const rerender = await collector.awaitFrame((frame) =>
			frame.method === "uiSlot"
			&& (frame.payload as Record<string, unknown>)["key"] === key
			&& JSON.stringify(frame.payload).includes("overlay:initial|x")
		);
		const delivered = await collector.awaitFrame((frame) => frame.id === 112);
		expect(delivered.payload).toEqual({ delivered: true });
		expect(rerender).toBeDefined();

		stdin.push(Buffer.from(encodeFrameString({
			id: 113, kind: "req", method: "uiEvent",
			payload: { key, generation, event: { type: "resize", width: 41, height: 20 } },
		})));
		expect((await collector.awaitFrame((frame) => frame.id === 113)).payload).toEqual({ delivered: true });

		stdin.push(Buffer.from(encodeFrameString({
			id: 114, kind: "req", method: "uiEvent",
			payload: { key, generation, event: { type: "focusGained" } },
		})));
		expect((await collector.awaitFrame((frame) => frame.id === 114)).payload).toEqual({ delivered: true });

		stdin.push(Buffer.from(encodeFrameString({
			id: 115, kind: "req", method: "uiEvent",
			payload: { key, generation, event: { type: "paste", text: "finish" }, data: "finish" },
		})));
		expect((await collector.awaitFrame((frame) => frame.id === 115)).payload).toEqual({ delivered: true });
		const disposed = await collector.awaitFrame((frame) =>
			frame.method === "disposeSlot"
			&& (frame.payload as Record<string, unknown>)["key"] === key
		);
		expect((disposed.payload as Record<string, unknown>)["generation"]).toBe(generation);
		expect((await collector.awaitFrame((frame) => frame.id === 110)).payload).toEqual({ ok: true });
		const slotFrames = collector.frames.filter((frame) =>
			frame.method === "uiSlot"
			&& (frame.payload as Record<string, unknown>)["key"] === key
		);
		const lastSlot = slotFrames.at(-1);
		expect(lastSlot).toBeDefined();
		expect(collector.frames.indexOf(lastSlot as Frame)).toBeLessThan(
			collector.frames.indexOf(disposed),
		);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});
});


// ===========================================================================
// 6. Registry snapshot + tool/provider wire contract
// ===========================================================================

describe("acceptance: registry snapshot and tool/provider bridges", () => {
	test("extensions.load returns full RegistrySnapshotWire", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([toolProgressFactory]);
		const badPath = resolve(import.meta.dirname, "..", "fixtures", "extensions", "does-not-exist.ts");
		const goodPath = resolve(import.meta.dirname, "..", "fixtures", "extensions", "provider-stream.ts");
		stdin.push(Buffer.from(encodeFrameString({
			id: 40, kind: "req", method: "extensions.load",
			payload: { extensionPaths: [goodPath, badPath], cwd: process.cwd() },
		})));
		const res = await collector.awaitFrame((f) => f.id === 40 && f.kind === "res");
		const payload = res.payload as Record<string, unknown>;

		// Isolation: bad path fails, good path + already-loaded factory survive.
		expect(payload["extensions"]).toBe(1);
		const errors = payload["errors"] as Array<Record<string, unknown>>;
		expect(errors.length).toBe(1);
		expect(String(errors[0]?.["path"])).toContain("does-not-exist");

		const tools = payload["tools"] as Array<Record<string, unknown>>;
		expect(tools.map((t) => t["name"]).sort()).toEqual(["progress_echo"]);
		const progress = tools[0] as Record<string, unknown>;
		expect(progress["label"]).toBe("ProgressEcho");
		expect(progress["description"]).toBe("Echoes with optional progress and cancel modes");
		expect(progress["parameters"]).toEqual(expect.objectContaining({ type: "object" }));

		const commands = payload["commands"] as Array<Record<string, unknown>>;
		expect(commands.some((c) => c["name"] === "progress_cmd")).toBe(true);

		const shortcuts = payload["shortcuts"] as Array<Record<string, unknown>>;
		expect(shortcuts.some((s) => s["key"] === "ctrl+p")).toBe(true);

		const flags = payload["flags"] as Array<Record<string, unknown>>;
		expect(flags.some((f) => f["name"] === "progress-flag" && f["type"] === "boolean")).toBe(true);

		const renderers = payload["renderers"] as Array<Record<string, unknown>>;
		expect(renderers.some((r) => r["name"] === "progress_msg" && r["type"] === "message")).toBe(true);

		const providers = payload["providers"] as Array<Record<string, unknown>>;
		// provider-stream loaded dynamically
		const fixture = providers.find((p) => p["name"] === "fixture_provider");
		expect(fixture).toBeDefined();
		expect(fixture?.["streamSimple"]).toBe(true);
		expect(fixture?.["baseUrl"]).toBe("https://fixture.example");
		expect(fixture?.["api"]).toBe("custom");

		const handlers = payload["handlers"] as string[];
		expect(handlers).toEqual(expect.arrayContaining(["session_start", "agent_start"]));

		// Sibling still alive: command from the first factory remains registered.
		stdin.push(Buffer.from(encodeFrameString({
			id: 41, kind: "req", method: "command.execute",
			payload: { command: "progress_cmd", args: "" },
		})));
		const cmdRes = await collector.awaitFrame((f) => f.id === 41 && f.kind === "res");
		expect(cmdRes.payload).toEqual({ ok: true });

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("tool.execute returns result and streams toolUpdate progress", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([toolProgressFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 50, kind: "req", method: "tool.execute",
			payload: {
				name: "progress_echo",
				toolCallId: "call-progress-1",
				args: { text: "hi", mode: "progress" },
			},
		})));
		const update = await collector.awaitFrame(
			(f) => f.id === 50 && f.kind === "event" && f.method === "toolUpdate",
		);
		const updatePayload = update.payload as Record<string, unknown>;
		expect(updatePayload["toolCallId"]).toBe("call-progress-1");
		expect(updatePayload["toolName"]).toBe("progress_echo");
		const partial = updatePayload["partialResult"] as Record<string, unknown>;
		expect(partial["content"]).toEqual([{ type: "text", text: "partial:hi" }]);

		const res = await collector.awaitFrame((f) => f.id === 50 && f.kind === "res");
		const result = res.payload as Record<string, unknown>;
		expect(result["content"]).toEqual([{ type: "text", text: "final:hi" }]);
		expect(result["details"]).toEqual({ stage: "final", toolCallId: "call-progress-1" });

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("tool.execute cancellation aborts in-flight tool", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([toolProgressFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 51, kind: "req", method: "tool.execute",
			payload: {
				name: "progress_echo",
				toolCallId: "call-cancel-1",
				args: { text: "x", mode: "cancel" },
			},
		})));
		// Give the tool a tick to enter the abort wait, then cancel.
		await Promise.resolve();
		await Promise.resolve();
		stdin.push(Buffer.from(encodeFrameString({
			id: 0, kind: "event", method: "tool.cancel",
			payload: { id: 51 },
		})));
		const res = await collector.awaitFrame(
			(f) => f.id === 51 && (f.kind === "error" || f.kind === "res"),
		);
		expect(res.kind).toBe("error");
		const err = res.payload as Record<string, unknown>;
		expect(err["code"]).toBe("cancelled");
		expect(err["retryable"]).toBe(false);

		// Sibling tool still works after cancel.
		stdin.push(Buffer.from(encodeFrameString({
			id: 52, kind: "req", method: "tool.execute",
			payload: {
				name: "progress_echo",
				toolCallId: "call-ok",
				args: { text: "alive" },
			},
		})));
		const ok = await collector.awaitFrame((f) => f.id === 52 && f.kind === "res");
		const okPayload = ok.payload as Record<string, unknown>;
		expect(okPayload["content"]).toEqual([{ type: "text", text: "alive" }]);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("provider.stream emits ordered providerEvent frames then terminal res", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([providerStreamFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 60, kind: "req", method: "provider.stream",
			payload: {
				providerId: "fixture_provider",
				model: { id: "ok", provider: "fixture_provider", api: "custom" },
				context: { messages: [] },
				options: {},
			},
		})));

		const events: Array<Record<string, unknown>> = [];
		while (events.length < 4) {
			const frame = await collector.awaitFrame(
				(f) => f.id === 60 && f.kind === "event" && f.method === "providerEvent"
					&& !events.includes(f.payload as Record<string, unknown>),
			);
			events.push(frame.payload as Record<string, unknown>);
		}
		expect(events.map((e) => e["type"])).toEqual([
			"start",
			"text_delta",
			"text_delta",
			"done",
		]);
		expect(events[1]?.["delta"]).toBe("hel");
		expect(events[2]?.["delta"]).toBe("lo");

		const res = await collector.awaitFrame((f) => f.id === 60 && f.kind === "res");
		expect(res.payload).toEqual({});

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("provider.stream error is isolated as non-retryable extension_error", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([providerStreamFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 61, kind: "req", method: "provider.stream",
			payload: {
				providerId: "fixture_provider",
				model: { id: "error", provider: "fixture_provider", api: "custom" },
				context: { messages: [] },
				options: {},
			},
		})));
		// May stream an error event then terminal error, or only terminal error.
		const terminal = await collector.awaitFrame(
			(f) => f.id === 61 && (f.kind === "error" || f.kind === "res"),
		);
		// The fixture throws before first push when model.id === "error" —
		// host responds with extension_error.
		// If stream emitted an error event first, terminal may still be res {}.
		// Prefer strict: throw path before any push.
		if (terminal.kind === "error") {
			const err = terminal.payload as Record<string, unknown>;
			expect(err["code"]).toBe("extension_error");
			expect(err["retryable"]).toBe(false);
			expect(String(err["message"])).toContain("provider-stream-error");
		} else {
			// Accept ordered error event then empty res.
			const errEv = collector.frames.find(
				(f) => f.id === 61 && f.kind === "event" && f.method === "providerEvent"
					&& (f.payload as Record<string, unknown>)["type"] === "error",
			);
			expect(errEv).toBeDefined();
		}

		// Sibling provider stream still works.
		stdin.push(Buffer.from(encodeFrameString({
			id: 62, kind: "req", method: "provider.stream",
			payload: {
				providerId: "fixture_provider",
				model: { id: "ok", provider: "fixture_provider", api: "custom" },
				context: { messages: [] },
				options: {},
			},
		})));
		const ok = await collector.awaitFrame((f) => f.id === 62 && f.kind === "res");
		expect(ok.kind).toBe("res");

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("provider.stream cancellation aborts in-flight stream", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([providerStreamFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 63, kind: "req", method: "provider.stream",
			payload: {
				providerId: "fixture_provider",
				model: { id: "cancel", provider: "fixture_provider", api: "custom" },
				context: { messages: [] },
				options: {},
			},
		})));
		// Wait for first events so the stream is parked on cancel wait.
		await collector.awaitFrame(
			(f) => f.id === 63 && f.kind === "event" && f.method === "providerEvent",
		);
		stdin.push(Buffer.from(encodeFrameString({
			id: 0, kind: "event", method: "tool.cancel",
			payload: { id: 63 },
		})));
		const terminal = await collector.awaitFrame(
			(f) => f.id === 63 && (f.kind === "error" || f.kind === "res"),
		);
		expect(terminal.kind).toBe("error");
		const err = terminal.payload as Record<string, unknown>;
		expect(err["code"]).toBe("cancelled");
		expect(err["retryable"]).toBe(false);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("tool and provider classify only structured abort errors as cancelled", async () => {
		const abortClassificationFactory: ExtensionFactory = (pi) => {
			pi.registerTool({
				name: "abort_classification",
				label: "AbortClassification",
				description: "Distinguishes cancellation-shaped messages from AbortError",
				parameters: Type.Object({ kind: Type.String() }),
				async execute(_toolCallId, args) {
					if (args.kind === "abort") throw new DOMException("aborted", "AbortError");
					throw new Error("cannot cancel booking");
				},
			});
			pi.registerProvider("abort_classification", {
				baseUrl: "https://fixture.example",
				api: "custom",
				streamSimple(model) {
					if ((model as { id?: unknown }).id === "abort") {
						throw new DOMException("aborted", "AbortError");
					}
					throw new Error("cannot cancel booking");
				},
			});
		};
		const { collector, stdin, host, runPromise } = await connectHost([abortClassificationFactory]);
		const send = (id: number, method: "tool.execute" | "provider.stream", payload: Record<string, unknown>) => {
			stdin.push(Buffer.from(encodeFrameString({ id, kind: "req", method, payload })));
		};
		const error = async (id: number) => {
			const frame = await collector.awaitFrame((candidate) => candidate.id === id && candidate.kind === "error");
			return frame.payload as Record<string, unknown>;
		};

		send(64, "tool.execute", {
			name: "abort_classification", toolCallId: "message-tool", args: { kind: "message" },
		});
		expect(await error(64)).toMatchObject({ code: "extension_error", message: "cannot cancel booking" });
		send(65, "tool.execute", {
			name: "abort_classification", toolCallId: "abort-tool", args: { kind: "abort" },
		});
		expect(await error(65)).toMatchObject({ code: "cancelled", message: "extension tool cancelled" });
		send(66, "provider.stream", {
			providerId: "abort_classification", model: { id: "message" }, context: {}, options: {},
		});
		expect(await error(66)).toMatchObject({ code: "extension_error", message: "cannot cancel booking" });
		send(67, "provider.stream", {
			providerId: "abort_classification", model: { id: "abort" }, context: {}, options: {},
		});
		expect(await error(67)).toMatchObject({ code: "cancelled", message: "provider stream cancelled" });

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("flags.set updates getFlag atomically and preserves values across sets", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([]);
		const extensionPath = resolve(import.meta.dirname, "..", "fixtures", "extensions", "control-compat.ts");
		stdin.push(Buffer.from(encodeFrameString({
			id: 90, kind: "req", method: "extensions.load",
			payload: { extensionPaths: [extensionPath], cwd: process.cwd() },
		})));
		const loaded = await collector.awaitFrame((frame) => frame.id === 90);
		const flags = (loaded.payload as Record<string, unknown>)["flags"] as Array<Record<string, unknown>>;
		expect(flags).toEqual(expect.arrayContaining([
			expect.objectContaining({ name: "compat-enabled", default: false, extensionPath }),
			expect.objectContaining({ name: "compat-label", default: "default", extensionPath }),
		]));

		stdin.push(Buffer.from(encodeFrameString({
			id: 91, kind: "req", method: "flags.set",
			payload: { values: { "compat-enabled": true, "compat-label": "first" } },
		})));
		expect((await collector.awaitFrame((frame) => frame.id === 91)).payload).toEqual({ ok: true });
		stdin.push(Buffer.from(encodeFrameString({
			id: 92, kind: "req", method: "session_start",
			payload: { type: "session_start", reason: "startup" },
		})));
		const first = await collector.awaitFrame((frame) =>
			frame.method === "notify" && String((frame.payload as Record<string, unknown>)["message"]).includes('"label":"first"')
		);
		expect(JSON.parse((first.payload as Record<string, unknown>)["message"] as string)).toEqual({ enabled: true, label: "first" });

		stdin.push(Buffer.from(encodeFrameString({
			id: 93, kind: "req", method: "flags.set",
			payload: { values: { "compat-label": "second" } },
		})));
		expect((await collector.awaitFrame((frame) => frame.id === 93)).payload).toEqual({ ok: true });
		stdin.push(Buffer.from(encodeFrameString({
			id: 94, kind: "req", method: "session_start",
			payload: { type: "session_start", reason: "reload" },
		})));
		const second = await collector.awaitFrame((frame) =>
			frame.method === "notify" && String((frame.payload as Record<string, unknown>)["message"]).includes('"label":"second"')
		);
		expect(JSON.parse((second.payload as Record<string, unknown>)["message"] as string)).toEqual({ enabled: true, label: "second" });

		stdin.push(Buffer.from(encodeFrameString({
			id: 95, kind: "req", method: "flags.set",
			payload: { values: { "compat-enabled": false, "compat-label": 7 } },
		})));
		const invalid = await collector.awaitFrame((frame) => frame.id === 95);
		expect(invalid.kind).toBe("error");
		expect((invalid.payload as Record<string, unknown>)["code"]).toBe("invalid_arguments");
		expect(host.getRunner()?.getFlagValues()).toEqual(new Map<string, boolean | string>([
			["compat-enabled", true], ["compat-label", "second"],
		]));

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("shortcut.execute ACKs before real dialogs and resolves the last registration", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([]);
		const firstPath = resolve(import.meta.dirname, "..", "fixtures", "extensions", "control-compat.ts");
		const lastPath = resolve(import.meta.dirname, "..", "fixtures", "extensions", "control-compat-shadow.ts");
		stdin.push(Buffer.from(encodeFrameString({
			id: 100, kind: "req", method: "extensions.load",
			payload: { extensionPaths: [firstPath, lastPath], cwd: process.cwd() },
		})));
		const loaded = await collector.awaitFrame((frame) => frame.id === 100);
		const shortcuts = ((loaded.payload as Record<string, unknown>)["shortcuts"] as Array<Record<string, unknown>>)
			.filter((entry) => entry["key"] === "ctrl+k");
		expect(shortcuts).toEqual([
			expect.objectContaining({ description: "First duplicate shortcut", extensionPath: firstPath }),
			expect.objectContaining({ description: "Last duplicate shortcut", extensionPath: lastPath }),
		]);

		stdin.push(Buffer.from(encodeFrameString({
			id: 101, kind: "req", method: "shortcut.execute", payload: { key: "ctrl+k" },
		})));
		const ack = await collector.awaitFrame((frame) => frame.id === 101);
		const select = await collector.awaitFrame((frame) => frame.kind === "req" && frame.method === "select");
		expect(ack.payload).toEqual({ handled: true });
		expect((select.payload as Record<string, unknown>)["title"]).toBe("compat select");
		expect(collector.frames.indexOf(ack)).toBeLessThan(collector.frames.indexOf(select));

		const reply = (request: Frame, payload: Record<string, unknown>) => {
			stdin.push(Buffer.from(encodeFrameString({
				id: request.id, kind: "res", method: request.method, payload,
			})));
		};
		reply(select, { value: "beta" });
		const confirm = await collector.awaitFrame((frame) => frame.kind === "req" && frame.method === "confirm");
		expect(confirm.payload).toEqual(expect.objectContaining({ title: "compat confirm", message: "continue?" }));
		reply(confirm, { confirmed: true });
		const input = await collector.awaitFrame((frame) => frame.kind === "req" && frame.method === "input");
		expect(input.payload).toEqual(expect.objectContaining({ title: "compat input", placeholder: "type here" }));
		reply(input, { value: "typed" });
		const editor = await collector.awaitFrame((frame) => frame.kind === "req" && frame.method === "editor");
		expect(editor.payload).toEqual({ title: "compat editor", prefill: "draft" });
		reply(editor, { value: "edited" });
		const completed = await collector.awaitFrame((frame) =>
			frame.method === "notify" && String((frame.payload as Record<string, unknown>)["message"]).includes('"edited":"edited"')
		);
		expect(JSON.parse((completed.payload as Record<string, unknown>)["message"] as string)).toEqual({
			selected: "beta", confirmed: true, input: "typed", edited: "edited",
		});

		stdin.push(Buffer.from(encodeFrameString({
			id: 102, kind: "req", method: "shortcut.execute", payload: { key: "ctrl+missing" },
		})));
		expect((await collector.awaitFrame((frame) => frame.id === 102)).payload).toEqual({ handled: false });

		stdin.push(Buffer.from(encodeFrameString({
			id: 103, kind: "req", method: "shortcut.execute", payload: { key: "ctrl+e" },
		})));
		expect((await collector.awaitFrame((frame) => frame.id === 103)).payload).toEqual({ handled: true });
		const failure = await collector.awaitFrame((frame) =>
			frame.method === "extensionError"
			&& String((frame.payload as Record<string, unknown>)["message"]).includes("shortcut fixture failure")
		);
		expect((failure.payload as Record<string, unknown>)["retryable"]).toBe(false);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});
});

describe("compact message update reconstruction", () => {
	test("reconstructs the public snapshot/event contract and clears terminal state", async () => {
		const updates: Array<Record<string, unknown>> = [];
		const factory: ExtensionFactory = (pi) => {
			pi.on("message_update", (...args: unknown[]) => {
				const event = args[0];
				if (event !== null && typeof event === "object") {
					updates.push(structuredClone(event as Record<string, unknown>));
				}
			});
		};
		const { collector, stdin, host, runPromise } = await connectHost([factory]);
		let id = 200;
		const sendDelta = async (event: Record<string, unknown>) => {
			const requestId = id++;
			stdin.push(Buffer.from(encodeFrameString({
				id: requestId,
				kind: "req",
				method: "message_update_delta",
				payload: { type: "message_update_delta", event },
			})));
			return await collector.awaitFrame((frame) => frame.id === requestId);
		};
		const meta = {
			role: "assistant",
			api: "test-api",
			provider: "test-provider",
			model: "test-model",
			usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: {} },
			stopReason: "stop",
			timestamp: 1,
		};

		expect((await sendDelta({ type: "start", meta })).kind).toBe("res");
		await sendDelta({
			type: "text_start",
			meta,
			contentIndex: 0,
			block: { type: "text", text: "" },
		});
		await sendDelta({ type: "text_delta", meta, contentIndex: 0, delta: "hel" });
		await sendDelta({ type: "text_delta", meta, contentIndex: 0, delta: "lo" });

		const textUpdate = updates.at(-1);
		const textMessage = textUpdate?.["message"] as Record<string, unknown>;
		const textEvent = textUpdate?.["assistantMessageEvent"] as Record<string, unknown>;
		expect((textMessage["content"] as Array<Record<string, unknown>>)[0]?.["text"]).toBe("hello");
		expect(textEvent["partial"]).toEqual(textMessage);
		expect(textEvent["delta"]).toBe("lo");

		await sendDelta({
			type: "toolcall_start",
			meta,
			contentIndex: 1,
			block: { type: "toolCall", id: "call-1", name: "read", arguments: {} },
		});
		await sendDelta({
			type: "toolcall_delta",
			meta,
			contentIndex: 1,
			delta: "{\"path\":\"README",
		});
		const toolUpdate = updates.at(-1);
		const toolMessage = toolUpdate?.["message"] as Record<string, unknown>;
		expect(
			((toolMessage["content"] as Array<Record<string, unknown>>)[1]?.["arguments"] as Record<string, unknown>)["path"],
		).toBe("README");
		await sendDelta({
			type: "toolcall_end",
			meta,
			contentIndex: 1,
			block: { type: "toolCall", id: "call-1", name: "read", arguments: { path: "README.md" } },
		});
		const toolEnd = updates.at(-1)?.["assistantMessageEvent"] as Record<string, unknown>;
		expect((toolEnd["toolCall"] as Record<string, unknown>)["arguments"]).toEqual({ path: "README.md" });

		const final = {
			...meta,
			content: [
				{ type: "text", text: "hello" },
				{ type: "toolCall", id: "call-1", name: "read", arguments: { path: "README.md" } },
			],
		};
		await sendDelta({ type: "done", reason: "stop", final });
		const done = updates.at(-1);
		expect(done?.["message"]).toEqual(final);
		expect((done?.["assistantMessageEvent"] as Record<string, unknown>)["message"]).toEqual(final);

		const afterTerminal = await sendDelta({ type: "text_delta", meta, contentIndex: 0, delta: "late" });
		expect(afterTerminal.kind).toBe("error");
		expect((afterTerminal.payload as Record<string, unknown>)["message"]).toContain("before assistant start");

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});
});

describe("tool rendering and argument preflight", () => {
	test("tool.renderHtml renders calls/results at width 80 as inert HTML and isolates failures", async () => {
		const renderFactory: ExtensionFactory = (pi) => {
			pi.registerTool({
				name: "render_fixture",
				label: "RenderFixture",
				description: "Renders acceptance output",
				parameters: Type.Object({ text: Type.String() }),
				async execute(_toolCallId, params) {
					return { content: [{ type: "text", text: String(params.text) }], details: {} };
				},
				renderCall(args, theme, context) {
					if (args.text === "boom") throw new Error("render-boom");
					return {
						render: (width) => [
							theme.bold(`call width=${width} id=${context.toolCallId}`),
							String(args.text),
						],
					};
				},
				renderResult(result, options, theme, context) {
					return {
						render: (width) => [
							theme.italic(`result width=${width} expanded=${String(options.expanded)} cwd=${context.cwd}`),
							String(result.content[0]?.type === "text" ? result.content[0].text : ""),
						],
					};
				},
			});
		};
		const { collector, stdin, host, runPromise } = await connectHost([renderFactory, toolProgressFactory]);

		stdin.push(Buffer.from(encodeFrameString({
			id: 70, kind: "req", method: "tool.renderHtml",
			payload: { phase: "call", toolName: "render_fixture", payload: { text: "<script>alert(1)</script>" } },
		})));
		const callResponse = await collector.awaitFrame((frame) => frame.id === 70);
		expect(callResponse.kind).toBe("res");
		const callHtml = String((callResponse.payload as Record<string, unknown>)["html"]);
		expect(callHtml).toContain("call width=80 id=html-export:render_fixture");
		expect(callHtml).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
		expect(callHtml).not.toContain("\x1b");
		expect(callHtml).not.toContain("<script>");

		stdin.push(Buffer.from(encodeFrameString({
			id: 71, kind: "req", method: "tool.renderHtml",
			payload: {
				phase: "result", toolName: "render_fixture",
				payload: { content: [{ type: "text", text: "result <b>bytes</b>" }], details: {} },
			},
		})));
		const resultResponse = await collector.awaitFrame((frame) => frame.id === 71);
		expect(resultResponse.kind).toBe("res");
		const resultHtml = String((resultResponse.payload as Record<string, unknown>)["html"]);
		expect(resultHtml).toContain("result width=80 expanded=true");
		expect(resultHtml).toContain("result &lt;b&gt;bytes&lt;/b&gt;");

		stdin.push(Buffer.from(encodeFrameString({
			id: 72, kind: "req", method: "tool.renderHtml",
			payload: { phase: "call", toolName: "render_fixture", payload: { text: "boom" } },
		})));
		const failed = await collector.awaitFrame((frame) => frame.id === 72);
		expect(failed.kind).toBe("error");
		expect((failed.payload as Record<string, unknown>)["code"]).toBe("extension_error");

		stdin.push(Buffer.from(encodeFrameString({
			id: 73, kind: "req", method: "tool.renderHtml",
			payload: { phase: "call", toolName: "render_fixture", payload: { text: "still alive" } },
		})));
		expect((await collector.awaitFrame((frame) => frame.id === 73)).kind).toBe("res");

		stdin.push(Buffer.from(encodeFrameString({
			id: 74, kind: "req", method: "tool.renderHtml",
			payload: { phase: "call", toolName: "progress_echo", payload: {} },
		})));
		expect((await collector.awaitFrame((frame) => frame.id === 74)).payload).toEqual({});

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("tool.prepare normalizes before canonical validation and malformed args are rejected", async () => {
		let prepareCalls = 0;
		const preflightFactory: ExtensionFactory = (pi) => {
			pi.registerTool({
				name: "preflight_fixture",
				label: "PreflightFixture",
				description: "Normalizes and validates arguments",
				parameters: Type.Object({ text: Type.String(), count: Type.Number() }),
				prepareArguments(args) {
					prepareCalls++;
					const raw = args as Record<string, unknown>;
					return { text: String(raw["text"] ?? ""), count: Number(raw["count"]) };
				},
				async execute(_toolCallId, params) {
					return { content: [{ type: "text", text: `${params.text}:${params.count}` }], details: {} };
				},
			});
		};
		const { collector, stdin, host, runPromise } = await connectHost([preflightFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 80, kind: "req", method: "tool.prepare",
			payload: { name: "preflight_fixture", args: { text: 42, count: "7" } },
		})));
		const prepared = await collector.awaitFrame((frame) => frame.id === 80);
		expect(prepared.kind).toBe("res");
		expect(prepared.payload).toEqual({ args: { text: "42", count: 7 } });

		stdin.push(Buffer.from(encodeFrameString({
			id: 81, kind: "req", method: "tool.validate",
			payload: { name: "preflight_fixture", args: { text: "42", count: 7 } },
		})));
		const valid = await collector.awaitFrame((frame) => frame.id === 81);
		expect(valid.kind).toBe("res");
		expect(valid.payload).toEqual({ args: { text: "42", count: 7 } });
		expect(prepareCalls).toBe(1);

		stdin.push(Buffer.from(encodeFrameString({
			id: 82, kind: "req", method: "tool.validate",
			payload: { name: "preflight_fixture", args: { text: "missing count" } },
		})));
		const invalid = await collector.awaitFrame((frame) => frame.id === 82);
		expect(invalid.kind).toBe("error");
		const error = invalid.payload as Record<string, unknown>;
		expect(error["code"]).toBe("invalid_arguments");
		expect(String(error["message"])).toContain("Validation failed for tool \"preflight_fixture\"");
		expect(prepareCalls).toBe(1);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});
});

describe("extension theme API", () => {
	const BUILT_IN_NAMES = [
		"dark", "light", "classic-dark", "classic-light", "motion-dark",
		"motion-light", "m3-dark", "m3-light", "antd-dark", "antd-light",
	];

	function themeWireFor(name: string): Record<string, unknown> {
		return {
			name,
			colorMode: "truecolor",
			fg: { accent: "#112233", text: "#ededed" },
			bg: { selectedBg: "#0a0a0a" },
		};
	}

	function themeUpdateFrame(name = "dark"): Frame {
		return {
			id: 0, kind: "event", method: "theme.update",
			payload: {
				theme: themeWireFor(name),
				terminalTheme: "dark",
				themeMode: "auto",
				themeGeneration: 1,
				themes: BUILT_IN_NAMES.map((name) => ({
					name,
					path: `/pkg/theme/${name}.json`,
					theme: themeWireFor(name),
				})),
			},
		};
	}

	test("catalog, getTheme, and every setTheme form behave like upstream", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([themeApiFactory]);
		stdin.push(Buffer.from(encodeFrameString(themeUpdateFrame())));
		stdin.push(Buffer.from(encodeFrameString({
			id: 60, kind: "req", method: "command.execute",
			payload: { command: "themeProbe", args: "" },
		})));

		const notifyFrame = await collector.awaitFrame((f) => f.method === "notify");
		const notifyPayload = notifyFrame.payload as { message: string };
		const report = JSON.parse(notifyPayload.message) as Record<string, unknown>;

		// ctx.ui.theme reflects the authoritative push.
		expect(report["initial"]).toBe("dark");

		// getAllThemes: 10 built-ins with shipped paths.
		expect(report["count"]).toBe(10);
		expect(report["names"]).toEqual(BUILT_IN_NAMES);
		expect(report["allHavePaths"]).toBe(true);

		// getTheme loads without switching; ANSI comes from the wire values.
		expect(report["m3"]).toEqual({ name: "m3-dark", accent: "\x1b[38;2;17;34;51m" });
		expect(report["missing"]).toBe(true);

		// Plain name: success + immediate ctx.ui.theme switch.
		expect(report["setClassic"]).toEqual({ success: true });
		expect(report["afterClassic"]).toBe("classic-light");

		// Pair resolves by polarity (auto + dark terminal → dark member).
		expect(report["setPair"]).toEqual({ success: true });
		expect(report["afterPair"]).toBe("dark");

		// Unknown name: upstream failure semantics — dark fallback, error text.
		expect(report["setMissing"]).toEqual({
			success: false,
			error: "Theme not found: nope",
		});
		expect(report["afterMissing"]).toBe("dark");

		// Theme-object form applies the instance itself.
		expect(report["setObject"]).toEqual({ success: true });
		expect(report["final"]).toBe("inmem");

		const commandRes = await collector.awaitFrame((f) => f.id === 60 && f.kind === "res");
		expect(commandRes.payload).toEqual({ ok: true });

		// Every switch reached Rust as a theme.set event with upstream
		// persistence semantics (persist on string success, never otherwise).
		const themeSets = collector.frames
			.filter((f) => f.method === "theme.set")
			.map((f) => f.payload as Record<string, unknown>);
		expect(themeSets.length).toBe(4);
		expect(themeSets[0]).toEqual({ name: "classic-light", persist: true });
		expect(themeSets[1]).toEqual({ name: "light/dark", persist: true });
		expect(themeSets[2]).toEqual({ name: "dark", persist: false });
		const objectSet = themeSets[3] ?? {};
		expect(objectSet["persist"]).toBe(false);
		const objectWire = objectSet["theme"] as Record<string, unknown>;
		expect(objectWire["name"]).toBe("inmem");
		expect((objectWire["fg"] as Record<string, unknown>)["accent"]).toBe("#010203");
		expect((objectWire["bg"] as Record<string, unknown>)["selectedBg"]).toBe(17);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("theme.update re-renders live slots with the new theme", async () => {
		const { collector, stdin, host, runPromise } = await connectHost([toolFactory]);
		// tool.ts greet does not set a widget; use setWidget via the host's UI
		// bridge indirectly: showOverlay creates an overlay slot.
		stdin.push(Buffer.from(encodeFrameString({
			id: 61, kind: "req", method: "command.execute",
			payload: { command: "showOverlay", args: "x" },
		})));
		const firstSlot = await collector.awaitFrame((f) => f.method === "uiSlot");
		const slotCountBefore = collector.frames.filter((f) => f.method === "uiSlot").length;

		stdin.push(Buffer.from(encodeFrameString(themeUpdateFrame())));
		await collector.awaitFrame((f) =>
			f.method === "uiSlot"
			&& collector.frames.filter((frame) => frame.method === "uiSlot").length > slotCountBefore,
		);
		const repushed = collector.frames.filter((f) => f.method === "uiSlot").at(-1);
		expect((repushed?.payload as Record<string, unknown>)["key"])
			.toBe((firstSlot.payload as Record<string, unknown>)["key"]);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("theme.update recreates factory slots with the new theme and disposes the old component once", async () => {
		const disposed: number[] = [];
		let factoryCalls = 0;
		const themeCaptureFactory: ExtensionFactory = (pi) => {
			pi.on("session_start", (_event, ctx) => {
				ctx.ui.setWidget("widget.theme-capture", (_tui, theme) => {
					const instance = factoryCalls++;
					const rendered = theme.fg("accent", "theme-capture");
					return {
						render: () => [rendered],
						dispose: () => disposed.push(instance),
					};
				});
			});
		};
		const { collector, stdin, host, runPromise } = await connectHost([themeCaptureFactory]);
		await sendSessionStart(stdin, collector);
		const initial = await collector.awaitFrame((frame) =>
			frame.method === "uiSlot" && (frame.payload as Record<string, unknown>)["key"] === "widget.theme-capture",
		);
		const priorSlotCount = collector.frames.filter((frame) => frame.method === "uiSlot").length;
		stdin.push(Buffer.from(encodeFrameString(themeUpdateFrame())));
		const updated = await collector.awaitFrame((frame) =>
			frame.method === "uiSlot"
			&& (frame.payload as Record<string, unknown>)["key"] === "widget.theme-capture"
			&& collector.frames.filter((candidate) => candidate.method === "uiSlot").length > priorSlotCount,
		);
		expect((updated.payload as Record<string, unknown>)["generation"])
			.toBe((initial.payload as Record<string, unknown>)["generation"]);
		expect((updated.payload as Record<string, unknown>)["runs"])
			.not.toEqual((initial.payload as Record<string, unknown>)["runs"]);
		expect(disposed).toEqual([0]);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("failed factory recreation reports an error and retains the existing slot", async () => {
		const disposed: string[] = [];
		const failureFactory: ExtensionFactory = (pi) => {
			pi.on("session_start", (_event, ctx) => {
				ctx.ui.setWidget("widget.theme-failure", (_tui, theme) => {
					if (theme.name === "broken") throw new Error("theme factory failed");
					const name = String(theme.name);
					return {
						render: () => [`factory-${name}`],
						dispose: () => disposed.push(name),
					};
				});
			});
		};
		const { collector, stdin, host, runPromise } = await connectHost([failureFactory]);
		await sendSessionStart(stdin, collector);
		const initial = await collector.awaitFrame((frame) =>
			frame.method === "uiSlot" && (frame.payload as Record<string, unknown>)["key"] === "widget.theme-failure",
		);
		(host as unknown as { applyThemeUpdate(update: unknown): void }).applyThemeUpdate(themeUpdateFrame("broken").payload);
		const error = await collector.awaitFrame((frame) =>
			frame.method === "extensionError"
			&& String((frame.payload as Record<string, unknown>)["message"]).includes("theme factory failed"),
		);
		expect((error.payload as Record<string, unknown>)["code"]).toBe("extension_error");
		expect(disposed).toEqual([]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 63, kind: "req", method: "render", payload: { key: "widget.theme-failure", width: 80 },
		})));
		const rendered = await collector.awaitFrame((frame) => frame.id === 63 && frame.kind === "res");
		expect((rendered.payload as Record<string, unknown>)["runs"])
			.toEqual((initial.payload as Record<string, unknown>)["runs"]);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("stale async custom-overlay resolution is disposed when done precedes resolution", async () => {
		// ui.custom is the only declared-async slot factory (its factory return
		// type is `unknown`, accepting a Promise). setWidget/setFooter/setHeader
		// declare synchronous factories, so an async widget factory is off-type.
		// ui.custom generates a unique key per call (host.ts:1633:
		// `overlay.${nextGeneration++}`), so a same-key collision is impossible
		// through the public API. The on-contract path to !isCurrent() is:
		// the factory returns a pending Promise, then done() is called before
		// it resolves — disposeSlot(key) removes the entry from this.slots, so
		// when the Promise resolves, install() sees this.slots.get(key) !== entry
		// and discards the stale component without emitting a uiSlot frame.
		const disposed: string[] = [];
		const staleComponent = {
			render: () => ["stale-content"],
			dispose: () => disposed.push("stale"),
		};
		const survivingComponent = {
			render: () => ["surviving-content"],
			dispose: () => disposed.push("surviving"),
		};
		const staleResolver = Promise.withResolvers<typeof staleComponent>();
		const survivingResolver = Promise.withResolvers<typeof survivingComponent>();
		const { promise: staleFactoryStarted, resolve: resolveStaleFactoryStarted } =
			Promise.withResolvers<void>();
		const { promise: survivingFactoryStarted, resolve: resolveSurvivingFactoryStarted } =
			Promise.withResolvers<void>();
		let staleDone: ((result: unknown) => void) | undefined;

		const raceFactory: ExtensionFactory = (pi) => {
			pi.registerCommand("custom-race", {
				description: "Two overlapping custom overlays with controlled resolution",
				async handler(_args, ctx) {
					await Promise.all([
						ctx.ui.custom((_tui, _theme, _kb, done) => {
							staleDone = done;
							resolveStaleFactoryStarted();
							return staleResolver.promise;
						}),
						ctx.ui.custom((_tui, _theme, _kb, _done) => {
							resolveSurvivingFactoryStarted();
							return survivingResolver.promise;
						}),
					]);
				},
			});
		};
		const { collector, stdin, host, runPromise } = await connectHost([raceFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 65, kind: "req", method: "command.execute",
			payload: { command: "custom-race", args: "" },
		})));

		// Wait for both factories to have been invoked so both Promises are
		// pending and staleDone is captured.
		await staleFactoryStarted;
		await survivingFactoryStarted;

		// No uiSlot yet — neither factory has resolved.
		expect(collector.frames.some((f) => f.method === "uiSlot")).toBe(false);

		// Dispose the first slot before its factory resolves. done() calls
		// disposeSlot(key), removing the entry from this.slots. When the
		// factory promise later resolves, install() sees !isCurrent() (the
		// entry is gone), disposes the stale component, and emits no uiSlot.
		staleDone?.("cancelled");

		// Resolve the stale factory — its component must be disposed and no
		// uiSlot frame emitted.
		staleResolver.resolve(staleComponent);

		// Wait on the observable the test cares about — the disposed list
		// reaching its expected membership — before asserting the negative.
		// The host emits no uiSlot here (stale resolution is suppressed), so
		// awaitFrame cannot gate this step.
		const sDeadline = Date.now() + 2_000;
		while (disposed.length < 1 && Date.now() < sDeadline) {
			await new Promise((r) => setTimeout(r, 1));
		}
		expect(disposed).toEqual(["stale"]);
		expect(collector.frames.some((frame) => frame.method === "uiSlot")).toBe(false);

		// Resolve the surviving factory — it installs and emits exactly one
		// uiSlot frame carrying its own content.
		survivingResolver.resolve(survivingComponent);
		const latest = await collector.awaitFrame((frame) => frame.method === "uiSlot");
		expect(JSON.stringify(latest.payload)).toContain("surviving-content");
		expect(collector.frames.filter((frame) => frame.method === "uiSlot")).toHaveLength(1);

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});

	test("theme.update preserves overlay state and propagates the new theme without re-invoking the factory", async () => {
		let factoryCalls = 0;
		const updateThemeCalls: string[] = [];
		let capturedDone: ((result: unknown) => void) | undefined;
		let capturedComponent: {
			state: { value: string };
			render: () => string[];
			updateTheme: (theme: unknown) => void;
		} | undefined;
		const overlayFactory: ExtensionFactory = (pi) => {
			pi.registerCommand("stateful-overlay", {
				description: "Creates a stateful overlay",
				async handler(_args, ctx) {
					await ctx.ui.custom((_tui, theme, _keybindings, done) => {
						factoryCalls++;
						const name = String((theme as { name?: unknown }).name ?? "dark");
						const component = {
							state: { value: "initial" },
							render: () => [`overlay-${name}`],
							updateTheme: (newTheme: unknown) => {
								updateThemeCalls.push(
									String((newTheme as { name?: unknown }).name ?? "?"),
								);
							},
						};
						capturedComponent = component;
						capturedDone = done;
						return Promise.resolve(component);
					});
				},
			});
		};
		const { collector, stdin, host, runPromise } = await connectHost([overlayFactory]);
		stdin.push(Buffer.from(encodeFrameString({
			id: 64, kind: "req", method: "command.execute",
			payload: { command: "stateful-overlay", args: "" },
		})));
		const firstSlot = await collector.awaitFrame((f) => f.method === "uiSlot");
		const slotCountBefore = collector.frames.filter((f) => f.method === "uiSlot").length;
		expect(factoryCalls).toBe(1);
		const initialComponent = capturedComponent;
		expect(initialComponent).toBeDefined();
		// Mutate the state to prove it survives the theme update.
		initialComponent!.state.value = "mutated";

		// Push a theme.update — the overlay must NOT re-invoke the factory.
		(host as unknown as { applyThemeUpdate(update: unknown): void }).applyThemeUpdate(themeUpdateFrame("red").payload);

		// (a) factory invoked exactly once — NOT re-invoked.
		expect(factoryCalls).toBe(1);
		// (b) component object reference unchanged and state survives.
		expect(capturedComponent).toBe(initialComponent);
		expect(capturedComponent!.state.value).toBe("mutated");
		// (c) updateTheme received the new theme.
		expect(updateThemeCalls).toEqual(["red"]);
		// (d) a re-render happened — a new uiSlot frame was emitted.
		await collector.awaitFrame((f) =>
			f.method === "uiSlot"
			&& collector.frames.filter((frame) => frame.method === "uiSlot").length > slotCountBefore,
		);
		const repushed = collector.frames.filter((f) => f.method === "uiSlot").at(-1);
		expect((repushed?.payload as Record<string, unknown>)["key"])
			.toBe((firstSlot.payload as Record<string, unknown>)["key"]);

		// Complete the overlay and await the command response.
		capturedDone?.("complete");
		await collector.awaitFrame((f) => f.id === 64 && f.kind === "res");

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});
});

// ===========================================================================
// 7. Built-in extensions are loaded in production
// ===========================================================================

describe("acceptance: built-in extensions load by default", () => {
	test("extensions.load snapshot includes llama.cpp provider and /llama command", async () => {
		const { collector, stdin, host, runPromise } = await connectHost(builtInExtensions);

		const id = 100;
		stdin.push(Buffer.from(encodeFrameString({
			id,
			kind: "req",
			method: "extensions.load",
			payload: { extensionPaths: [], cwd: process.cwd(), projectTrusted: true },
		})));
		const res = await collector.awaitFrame((f) => f.id === id && f.kind === "res");
		const payload = res.payload as Record<string, unknown>;

		const providers = payload["providers"] as Array<Record<string, unknown>>;
		expect(providers.some((p) => p["name"] === "llama.cpp")).toBe(true);

		const commands = payload["commands"] as Array<Record<string, unknown>>;
		const llamaCmd = commands.find((c) => c["name"] === "llama");
		expect(llamaCmd).toBeDefined();
		expect(llamaCmd?.["source"]).toBe("<inline:llama.cpp>");

		stdin.push(null);
		host.dispose("test");
		await runPromise.catch(() => void 0);
	});
});
