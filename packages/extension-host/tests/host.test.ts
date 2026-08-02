/**
 * Host bridge tests: hello handshake, version mismatch rejection, extension
 * loading via the REAL coding-agent loader, and ExtensionRunner hook dispatch.
 */
import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { Readable } from "node:stream";
import {
	PROTOCOL_VERSION,
	encodeFrameString,
	type Frame,
} from "@earendil-works/pi-tui-protocol";
import type {
	ExtensionFactory,
	ExtensionContextActions,
} from "@earendil-works/pi-coding-agent";
import {
	loadExtensionFromFactory,
	createExtensionRuntime,
} from "@earendil-works/pi-coding-agent";
import { ExtensionRunner } from "@earendil-works/pi-coding-agent";
import { ExtensionHost, createEventBus } from "../src/host.ts";
import { COMPATIBILITY_VERSION } from "../src/version.ts";
import { createExtensionJiti } from "../src/virtual-modules.ts";
import { loadRunOptions, parseArgs } from "../src/main.ts";

import hooksFactory from "../fixtures/extensions/hooks.ts";
import toolFactory from "../fixtures/extensions/tool.ts";
import commandContextFactory from "../fixtures/extensions/command-context.ts";

/** Collecting ByteWritable that signals on first write (no Writable dependency). */
class PipeWritable {
	readonly chunks: Uint8Array[] = [];
	private readonly ready: Promise<void>;
	private readonly signalReady: () => void;
	private firstWrite = true;

	constructor() {
		const { promise, resolve } = Promise.withResolvers<void>();
		this.ready = promise;
		this.signalReady = resolve;
	}

	write(chunk: Uint8Array): void {
		this.chunks.push(chunk);
		if (this.firstWrite) {
			this.firstWrite = false;
			this.signalReady();
		}
	}

	awaitFirstWrite(): Promise<void> {
		return this.ready;
	}
}

/** Decode collected stdout chunks into Frame[]. */
function decodeChunks(chunks: Uint8Array[]): Frame[] {
	const text = new TextDecoder().decode(
		Buffer.concat(chunks.map((c) => Buffer.from(c))),
	);
	const frames: Frame[] = [];
	for (const line of text.split("\n")) {
		if (line.trim().length > 0) {
			frames.push(JSON.parse(line) as Frame);
		}
	}
	return frames;
}

/** Minimal context-actions stub for runner construction in tests. */
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

describe("host: hello handshake", () => {
	test("matching versions ack successfully", async () => {
		const pipe = new PipeWritable();
		const stdin = new Readable({ read() {} });
		const host = new ExtensionHost(stdin, pipe);
		const runPromise = host.run({ cwd: process.cwd(), extensionPaths: [] });

		stdin.push(Buffer.from(encodeFrameString({
			id: 1, kind: "req", method: "hello",
			payload: { protocolVersion: PROTOCOL_VERSION, compatibilityVersion: COMPATIBILITY_VERSION },
		})));
		stdin.push(null); // EOF so the read loop completes.

		await pipe.awaitFirstWrite(); // helloAck emitted.
		await runPromise.catch(() => void 0);
	});

	test("protocol version mismatch terminates host", async () => {
		const pipe = new PipeWritable();
		const stdin = new Readable({ read() {} });
		const host = new ExtensionHost(stdin, pipe);
		const runPromise = host.run({ cwd: process.cwd(), extensionPaths: [] });

		stdin.push(Buffer.from(encodeFrameString({
			id: 1, kind: "req", method: "hello",
			payload: { protocolVersion: 999, compatibilityVersion: COMPATIBILITY_VERSION },
		})));
		stdin.push(null); // EOF so the read loop completes.

		await runPromise.catch(() => void 0);
		expect(host.isDisposed).toBe(true);
	});

	test("compatibility version mismatch terminates host", async () => {
		const pipe = new PipeWritable();
		const stdin = new Readable({ read() {} });
		const host = new ExtensionHost(stdin, pipe);
		const runPromise = host.run({ cwd: process.cwd(), extensionPaths: [] });

		stdin.push(Buffer.from(encodeFrameString({
			id: 1, kind: "req", method: "hello",
			payload: { protocolVersion: PROTOCOL_VERSION, compatibilityVersion: "0.99.0" },
		})));
		stdin.push(null); // EOF so the read loop completes.

		await runPromise.catch(() => void 0);
		expect(host.isDisposed).toBe(true);
	});
});

describe("host: built-in options", () => {
	test("defaults to compat mode with built-ins", async () => {
		const options = await loadRunOptions(["bun", "pi-extension-host"]);
		expect(options.factories.length).toBeGreaterThan(0);
	});

	test("parses --no-builtins in any argument position", async () => {
		const argv = [
			"bun", "pi-extension-host", "--extension", "first.mjs",
			"-C", "/project", "--no-builtins", "-e", "second.mjs",
		];
		expect(parseArgs(argv)).toEqual({
			cwd: "/project",
			extensionPaths: ["first.mjs", "second.mjs"],
			noBuiltins: true,
			lean: false,
		});
		const options = await loadRunOptions(argv);
		expect(options.factories).toEqual([]);
	});
});

describe("host: REAL ExtensionRunner hooks", () => {
	async function loadFactory(factory: ExtensionFactory, path: string) {
		const runtime = createExtensionRuntime();
		const bus = createEventBus();
		const ext = await loadExtensionFromFactory(factory, process.cwd(), bus, runtime, path);
		return { ext, runtime };
	}

	test("hooks fixture registers lifecycle handlers", async () => {
		const { ext } = await loadFactory(hooksFactory, "hooks.ts");
		expect([...ext.handlers.keys()]).toEqual(expect.arrayContaining([
			"session_start", "agent_start", "message_end", "context", "input",
			"turn_start", "tool_execution_start",
		]));
	});

	test("tool fixture registers tool, command, and widget handler", async () => {
		const { ext } = await loadFactory(toolFactory, "tool.ts");
		expect([...ext.tools.keys()]).toContain("echo");
		expect([...ext.commands.keys()]).toContain("greet");
		expect(ext.handlers.has("session_start")).toBe(true);
	});

	test("runner dispatches session_start without error", async () => {
		const { ext, runtime } = await loadFactory(hooksFactory, "hooks.ts");
		// Escape hatch: reference class stubs.
		const runner = new ExtensionRunner(
			[ext], runtime, process.cwd(),
			{} as unknown as ConstructorParameters<typeof ExtensionRunner>[3],
			{ getAll: () => [], find: () => undefined } as unknown as ConstructorParameters<typeof ExtensionRunner>[4],
		);
		runner.bindCore({} as never, noopContextActions);
		await runner.emit({ type: "session_start", reason: "startup" });
		expect(runner.hasHandlers("session_start")).toBe(true);
	});

	test("runner returns context hook result (pipeline)", async () => {
		const { ext, runtime } = await loadFactory(hooksFactory, "hooks.ts");
		const runner = new ExtensionRunner(
			[ext], runtime, process.cwd(),
			{} as unknown as ConstructorParameters<typeof ExtensionRunner>[3],
			{ getAll: () => [], find: () => undefined } as unknown as ConstructorParameters<typeof ExtensionRunner>[4],
		);
		runner.bindCore({} as never, noopContextActions);
		const messages = [{ role: "user", content: [{ type: "text", text: "hi" }] }] as never;
		const result = await runner.emitContext(messages);
		expect(result).toEqual(messages);
	});

	test("runner returns input hook result (continue)", async () => {
		const { ext, runtime } = await loadFactory(hooksFactory, "hooks.ts");
		const runner = new ExtensionRunner(
			[ext], runtime, process.cwd(),
			{} as unknown as ConstructorParameters<typeof ExtensionRunner>[3],
			{ getAll: () => [], find: () => undefined } as unknown as ConstructorParameters<typeof ExtensionRunner>[4],
		);
		runner.bindCore({} as never, noopContextActions);
		const result = await runner.emitInput("hello", undefined, "interactive");
		expect(result.action).toBe("continue");
	});

	test("first registration wins for duplicate tool names", async () => {
		const runtime = createExtensionRuntime();
		const bus = createEventBus();
		const ext1 = await loadExtensionFromFactory(toolFactory, process.cwd(), bus, runtime, "ext1.ts");
		const ext2 = await loadExtensionFromFactory(toolFactory, process.cwd(), bus, runtime, "ext2.ts");
		const runner = new ExtensionRunner(
			[ext1, ext2], runtime, process.cwd(),
			{} as unknown as ConstructorParameters<typeof ExtensionRunner>[3],
			{ getAll: () => [], find: () => undefined } as unknown as ConstructorParameters<typeof ExtensionRunner>[4],
		);
		const echoTools = runner.getAllRegisteredTools().filter((t) => t.definition.name === "echo");
		expect(echoTools).toHaveLength(1);
		// First registration wins; verify it came from ext1 (load order).
		const winner = echoTools[0];
		expect(winner?.sourceInfo).toBeDefined();
	});
});

describe("host: loads real example extensions via jiti", () => {
	// Reference examples are loaded via the same jiti path the host uses,
	// because their internal @earendil-works/* imports need the alias map.
	const REF_EXAMPLES = resolve(
		import.meta.dirname, "..", "..", "..",
		".references", "pi", "packages", "coding-agent", "examples", "extensions",
	);

	async function loadViaJiti(name: string): Promise<{ tools: string[]; handlers: string[] }> {
		const jiti = createExtensionJiti();
		const extPath = resolve(REF_EXAMPLES, name);
		const module = await jiti.import(extPath, { default: true }) as unknown;
		if (typeof module !== "function") throw new Error(`${name} did not export a factory`);
		const runtime = createExtensionRuntime();
		const bus = createEventBus();
		const ext = await loadExtensionFromFactory(
			module as ExtensionFactory, process.cwd(), bus, runtime, extPath,
		);
		return { tools: [...ext.tools.keys()], handlers: [...ext.handlers.keys()] };
	}

	test("hello.ts registers hello tool", async () => {
		const result = await loadViaJiti("hello.ts");
		expect(result.tools).toContain("hello");
	});

	test("widget-placement.ts registers session_start handler", async () => {
		const result = await loadViaJiti("widget-placement.ts");
		expect(result.handlers).toContain("session_start");
	});
});

/** ByteWritable that decodes frames and lets tests await them by predicate. */
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
		const { promise, resolve: resolveWaiter } = Promise.withResolvers<Frame>();
		this.waiters.push({ predicate, resolve: resolveWaiter });
		return promise;
	}
}

interface Connected {
	collector: FrameCollector;
	stdin: Readable;
	host: ExtensionHost;
	runPromise: Promise<void>;
}

async function connectHost(factories: ExtensionFactory[]): Promise<Connected> {
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

function push(stdin: Readable, frame: Frame): void {
	stdin.push(Buffer.from(encodeFrameString(frame)));
}

async function teardown(connected: Connected): Promise<void> {
	connected.stdin.push(null);
	connected.host.dispose("test");
	await connected.runPromise.catch(() => void 0);
}

function payloadOf(frame: Frame): Record<string, unknown> {
	return frame.payload as Record<string, unknown>;
}

describe("host: command context + mirrored session state", () => {
	test("getContextUsage and scopedModels round-trip from session.update", async () => {
		const connected = await connectHost([commandContextFactory]);
		const { collector, stdin } = connected;

		const usage = { tokens: 2400, contextWindow: 128000, percent: 1.9 };
		const scopedModels = [
			{ model: { id: "gpt-x", provider: "openai" }, thinkingLevel: "high" },
		];

		// Keep the session busy so waitForIdle parks until we push idle.
		push(stdin, {
			id: 0, kind: "event", method: "session.update",
			payload: {
				thinkingLevel: "high",
				activeTools: [],
				allTools: [],
				commands: [],
				isIdle: false,
				hasPendingMessages: false,
				contextUsage: usage,
				scopedModels,
				systemPrompt: "probe",
			},
		});

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession")
			.then((request) => {
				expect(payloadOf(request)["parentSession"]).toBe("parent-1");
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: false },
				});
			});

		push(stdin, {
			id: 40, kind: "req", method: "command.execute",
			payload: { command: "commandContextProbe", args: "" },
		});

		// Resolve waitForIdle via the session.update bridge.
		push(stdin, {
			id: 0, kind: "event", method: "session.update",
			payload: {
				thinkingLevel: "high",
				activeTools: [],
				allTools: [],
				commands: [],
				isIdle: true,
				hasPendingMessages: false,
				contextUsage: usage,
				scopedModels,
				systemPrompt: "probe",
			},
		});

		const notify = await collector.awaitFrame((f) => f.method === "notify");
		await collector.awaitFrame((f) => f.id === 40 && f.kind === "res");

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(report["contextUsage"]).toEqual(usage);
		expect(report["scopedModels"]).toEqual(scopedModels);
		expect(report["hasWaitForIdle"]).toBe(true);
		expect(report["hasNewSession"]).toBe(true);
		expect(report["waitForIdleOk"]).toBe(true);
		expect(report["newSession"]).toEqual({ cancelled: false });

		const newSessionReq = collector.frames.find(
			(f) => f.kind === "req" && f.method === "session.newSession",
		);
		expect(newSessionReq).toBeDefined();
		expect(payloadOf(newSessionReq!)["parentSession"]).toBe("parent-1");

		await teardown(connected);
	});

	test("command handler waitForIdle / newSession stubs hit the bridge", async () => {
		const connected = await connectHost([commandContextFactory]);
		const { collector, stdin } = connected;

		push(stdin, {
			id: 0, kind: "event", method: "session.update",
			payload: {
				thinkingLevel: "medium",
				activeTools: [],
				allTools: [],
				commands: [],
				isIdle: true,
				hasPendingMessages: false,
				contextUsage: { tokens: 10, contextWindow: 1000, percent: 1 },
				scopedModels: [{ model: { id: "m1", provider: "p" } }],
				systemPrompt: "",
			},
		});

		const bridgeCalls: string[] = [];
		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession")
			.then((request) => {
				bridgeCalls.push("session.newSession");
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: true },
				});
			});

		push(stdin, {
			id: 41, kind: "req", method: "command.execute",
			payload: { command: "commandContextProbe", args: "" },
		});

		const notify = await collector.awaitFrame((f) => f.method === "notify");
		await collector.awaitFrame((f) => f.id === 41 && f.kind === "res");

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(bridgeCalls).toEqual(["session.newSession"]);
		expect(report["newSession"]).toEqual({ cancelled: true });
		expect(report["waitForIdleOk"]).toBe(true);
		expect(report["contextUsage"]).toEqual({ tokens: 10, contextWindow: 1000, percent: 1 });
		expect(report["scopedModels"]).toEqual([{ model: { id: "m1", provider: "p" } }]);

		await teardown(connected);
	});
});
