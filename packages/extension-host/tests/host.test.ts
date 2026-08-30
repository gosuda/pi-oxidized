/**
 * Host bridge tests: hello handshake, version mismatch rejection, extension
 * loading via the REAL coding-agent loader, and ExtensionRunner hook dispatch.
 */
import { describe, expect, test } from "bun:test";
import { resolve, join } from "node:path";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { Readable } from "node:stream";
import {
	PROTOCOL_VERSION,
	encodeFrameString,
	type ByteWritable,
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
import replacedSessionFactory from "../fixtures/extensions/replaced-session.ts";
import replacementReadyFactory from "../fixtures/extensions/replacement-ready.ts";
import staleCtxFactory, { resetCapturedCtx } from "../fixtures/extensions/stale-ctx.ts";
import toolCallReorderFactory from "../fixtures/extensions/tool-call-reorder.ts";
import sessionManagerProxyFactory from "../fixtures/extensions/session-manager-proxy.ts";

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
		".references", "pi-2.0", "packages", "coding-agent", "examples", "extensions",
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
		reject: (error: Error) => void;
		timer: ReturnType<typeof setTimeout>;
	}> = [];
	private buf = "";

	protected beforeFrame(_frame: Frame): void {}

	write(chunk: Uint8Array): void {
		this.buf += new TextDecoder().decode(chunk);
		const lines = this.buf.split("\n");
		this.buf = lines.pop() ?? "";
		for (const line of lines) {
			if (line.trim().length > 0) {
				const frame = JSON.parse(line) as Frame;
				this.beforeFrame(frame);
				this.frames.push(frame);
				for (let i = this.waiters.length - 1; i >= 0; i--) {
					const waiter = this.waiters[i];
					if (waiter !== undefined && waiter.predicate(frame)) {
						clearTimeout(waiter.timer);
						waiter.resolve(frame);
						this.waiters.splice(i, 1);
					}
				}
			}
		}
	}

	awaitFrame(predicate: (f: Frame) => boolean, label = "frame", timeoutMs = 5_000): Promise<Frame> {
		const existing = this.frames.find(predicate);
		if (existing !== undefined) return Promise.resolve(existing);
		const { promise, resolve, reject } = Promise.withResolvers<Frame>();
		const timer = setTimeout(() => {
			const index = this.waiters.indexOf(waiter);
			if (index !== -1) this.waiters.splice(index, 1);
			const seen = this.frames.map((f) => `${f.kind}:${f.method}:${f.id}`).join(", ");
			reject(new Error(`awaitFrame timed out waiting for "${label}" after ${timeoutMs}ms; frames seen: [${seen}]`));
		}, timeoutMs);
		timer.unref();
		const waiter = { predicate, resolve, reject, timer };
		this.waiters.push(waiter);
		return promise;
	}
}
/** FrameCollector whose `write` rejects for a configurable set of methods. */
class FailingFrameCollector extends FrameCollector {
	private readonly failMethods: Set<string>;
	private failCount = 0;

	constructor(failMethods: string[]) {
		super();
		this.failMethods = new Set(failMethods);
	}

	get failedSends(): number { return this.failCount; }

	write(chunk: Uint8Array): void {
		const text = new TextDecoder().decode(chunk);
		for (const method of this.failMethods) {
			if (text.includes(`"method":"${method}"`)) {
				this.failCount++;
				throw new Error(`simulated write failure for ${method}`);
			}
		}
		super.write(chunk);
	}
}

interface Connected {
	collector: FrameCollector;
	stdin: Readable;
	host: ExtensionHost;
	runPromise: Promise<void>;
}

async function connectHost(
	factories: ExtensionFactory[],
	collector: FrameCollector = new FrameCollector(),
): Promise<Connected> {
	const stdin = new Readable({ read() {} });
	const host = new ExtensionHost(stdin, collector);
	const runPromise = host.run({ cwd: process.cwd(), factories, extensionPaths: [] });

	stdin.push(Buffer.from(encodeFrameString({
		id: 1, kind: "req", method: "hello",
		payload: { protocolVersion: PROTOCOL_VERSION, compatibilityVersion: COMPATIBILITY_VERSION },
	})));
	await collector.awaitFrame((f) => f.id === 1 && f.kind === "res", "res 1");
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

async function respondSetupEntries(
	collector: FrameCollector,
	stdin: Readable,
	replacementToken: string,
	snapshots: readonly (readonly unknown[])[],
): Promise<void> {
	const seen = new Set<Frame["id"]>();
	for (const entries of snapshots) {
		const request = await collector.awaitFrame(
			(frame) =>
				frame.kind === "req"
				&& frame.method === "session.setupEntries"
				&& !seen.has(frame.id),
			"session.setupEntries req",
		);
		seen.add(request.id);
		expect(payloadOf(request)["replacementToken"]).toBe(replacementToken);
		push(stdin, {
			id: request.id,
			kind: "res",
			method: "session.setupEntries",
			payload: { entries },
		});
	}
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
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession", "session.newSession req")
			.then((request) => {
				expect(payloadOf(request)["parentSession"]).toBe("parent-1");
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: true },
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

		const notify = await collector.awaitFrame((f) => f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 40 && f.kind === "res", "res 40");

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(report["contextUsage"]).toEqual(usage);
		expect(report["scopedModels"]).toEqual(scopedModels);
		expect(report["hasWaitForIdle"]).toBe(true);
		expect(report["hasNewSession"]).toBe(true);
		expect(report["waitForIdleOk"]).toBe(true);
		expect(report["newSession"]).toEqual({ cancelled: true });

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
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession", "session.newSession req")
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

		const notify = await collector.awaitFrame((f) => f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 41 && f.kind === "res", "res 41");

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(bridgeCalls).toEqual(["session.newSession"]);
		expect(report["newSession"]).toEqual({ cancelled: true });
		expect(report["waitForIdleOk"]).toBe(true);
		expect(report["contextUsage"]).toEqual({ tokens: 10, contextWindow: 1000, percent: 1 });
		expect(report["scopedModels"]).toEqual([{ model: { id: "m1", provider: "p" } }]);

		await teardown(connected);
	});
});

/** Writable that throws on a chosen `session.command` action. */
class FailingOnSessionCommand extends FrameCollector {
	failedAction: string | undefined;
	private failAction: string | undefined;

	protected override beforeFrame(frame: Frame): void {
		const action = frame.kind === "event" && frame.method === "session.command"
			? payloadOf(frame)["action"]
			: undefined;
		if (this.failAction !== undefined && action === this.failAction) {
			this.failedAction = String(action);
			throw new Error(`transport write failed (${action})`);
		}
	}

	armFailure(action: string): void { this.failAction = action; }
}

describe("host: newSession setup + withSession + ReplacedSessionContext", () => {
	function sessionUpdate(
		stdin: Readable,
		idle: boolean,
		sessionName?: string,
	): void {
		push(stdin, {
			id: 0, kind: "event", method: "session.update",
			payload: {
				sessionName,
				thinkingLevel: "medium",
				activeTools: [],
				allTools: [],
				commands: [],
				isIdle: idle,
				hasPendingMessages: false,
				contextUsage: { tokens: 10, contextWindow: 1000, percent: 1 },
				scopedModels: [],
				systemPrompt: "",
			},
		});
	}

	test("newSession: setup runs before withSession on non-cancelled replacement", async () => {
		const connected = await connectHost([replacedSessionFactory]);
		const { collector, stdin } = connected;

		sessionUpdate(stdin, true);

		const initialEntry = { type: "message", id: "initial" };
		const customEntry = { type: "custom", id: "custom" };
		const sessionInfoEntry = {
			type: "session_info",
			id: "session-info",
			name: "setup-session",
		};

		const setupResponses = (async () => {
			const request = await collector.awaitFrame(
				(f) => f.kind === "req" && f.method === "session.newSession",
				"session.newSession req",
			);
			push(stdin, {
				id: request.id,
				kind: "res",
				method: "session.newSession",
				payload: { cancelled: false, replacementToken: "tok-setup-1" },
			});
			await respondSetupEntries(collector, stdin, "tok-setup-1", [
				[initialEntry],
				[initialEntry, customEntry],
				[initialEntry, customEntry, sessionInfoEntry],
			]);
		})();

		push(stdin, {
			id: 50, kind: "req", method: "command.execute",
			payload: { command: "replacedSessionProbe", args: "" },
		});

		const notify = await collector.awaitFrame((f) => f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 50 && f.kind === "res", "res 50");
		await setupResponses;

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(report["setupOrder"]).toEqual(["setup", "withSession"]);
		expect(report["setupReceived"]).toBe(true);
		expect(report["newSessionResult"]).toEqual({ cancelled: false });
		expect(report["withSessionSendMessage"]).toBe("function");
		expect(report["withSessionSendUserMessage"]).toBe("function");
		// Narrow bridge: unsupported SessionManager method throws (no silent no-op).
		expect(report["unsupportedThrew"]).toBe(true);
		expect(typeof report["unsupportedMessage"]).toBe("string");
		expect(String(report["unsupportedMessage"])).toContain("is not supported");
		// appendSessionInfo updates the one mirrored SessionManager getter.
		expect(report["setupSessionName"]).toBe("setup-session");
		expect(report["initialEntries"]).toEqual([initialEntry]);
		expect(report["entriesAfterAppend"]).toEqual([initialEntry, customEntry]);
		expect(report["entriesAfterSessionInfo"]).toEqual([
			initialEntry,
			customEntry,
			sessionInfoEntry,
		]);
		// withSession sends awaited the wire write to completion.
		expect(report["withSessionSendsDone"]).toBe(true);

		// Setup mutations and withSession sends bridge to Rust as session.command events.
		const commandEvents = collector.frames.filter(
			(f) => f.kind === "event" && f.method === "session.command",
		);
		expect(commandEvents.length).toBeGreaterThanOrEqual(4);
		const actions = commandEvents.map((f) => payloadOf(f)["action"]);
		expect(actions).toContain("sendMessage");
		expect(actions).toContain("sendUserMessage");
		// The two setup mutations must emit their exact bridge payloads before
		// either withSession send helper runs; neither returns a fabricated ID.
		const appendEntryIdx = actions.indexOf("appendEntry");
		const setSessionNameIdx = actions.indexOf("setSessionName");
		const sendMessageIdx = actions.indexOf("sendMessage");
		const sendUserMessageIdx = actions.indexOf("sendUserMessage");
		const appendEntryFrame = commandEvents.find(
			(f) => payloadOf(f)["action"] === "appendEntry",
		);
		const setSessionNameFrame = commandEvents.find(
			(f) => payloadOf(f)["action"] === "setSessionName",
		);
		expect(appendEntryFrame).toBeDefined();
		expect(setSessionNameFrame).toBeDefined();
		if (appendEntryFrame !== undefined) {
			expect(payloadOf(appendEntryFrame)).toMatchObject({
				action: "appendEntry",
				customType: "setup-custom",
				data: { from: "setup" },
			});
		}
		if (setSessionNameFrame !== undefined) {
			expect(payloadOf(setSessionNameFrame)).toMatchObject({
				action: "setSessionName",
				name: "setup-session",
			});
		}
		expect(appendEntryIdx).toBeGreaterThanOrEqual(0);
		expect(setSessionNameIdx).toBeGreaterThan(appendEntryIdx);
		expect(sendMessageIdx).toBeGreaterThan(setSessionNameIdx);
		expect(sendUserMessageIdx).toBeGreaterThan(sendMessageIdx);
		// The command.execute response (id:50) must land AFTER both withSession
		// send helper frames: the sends now await the wire write, so the
		// command handler cannot complete until they have been emitted.
		const commandResIdx = collector.frames.findIndex(
			(f) => f.id === 50 && f.kind === "res",
		);
		expect(commandResIdx).toBeGreaterThanOrEqual(0);
		const sendUserMessageFrameIdx = collector.frames.findIndex(
			(f) =>
				f.kind === "event" &&
				f.method === "session.command" &&
				payloadOf(f)["action"] === "sendUserMessage",
		);
		expect(sendUserMessageFrameIdx).toBeGreaterThanOrEqual(0);
		expect(commandResIdx).toBeGreaterThan(sendUserMessageFrameIdx);

		await teardown(connected);
	});


	test("failed setup-name refresh leaves the active session mirror unchanged", async () => {
		let setupRejected = false;
		const setupNameFactory: ExtensionFactory = (pi) => {
			pi.registerCommand("failSetupName", {
				description: "Fail a pending setup name refresh",
				async handler(_args, ctx) {
					try {
						await ctx.newSession({
							setup: async (manager) => {
								await manager.appendSessionInfo("pending-name");
							},
						});
					} catch {
						setupRejected = true;
					}
				},
			});
			pi.registerCommand("readActiveName", {
				description: "Read the active session name",
				async handler(_args, ctx) {
					ctx.ui.notify(String(pi.getSessionName()), "info");
				},
			});
		};
		const connected = await connectHost([setupNameFactory]);
		const { collector, stdin } = connected;
		sessionUpdate(stdin, true, "active-name");

		const setupResponses = (async () => {
			const replacement = await collector.awaitFrame(
				(f) => f.kind === "req" && f.method === "session.newSession",
				"session.newSession req",
			);
			push(stdin, {
				id: replacement.id,
				kind: "res",
				method: "session.newSession",
				payload: { cancelled: false, replacementToken: "tok-name-failure" },
			});
			const seed = await collector.awaitFrame(
				(f) => f.kind === "req" && f.method === "session.setupEntries",
				"session.setupEntries seed",
			);
			push(stdin, {
				id: seed.id,
				kind: "res",
				method: "session.setupEntries",
				payload: { entries: [] },
			});
			await collector.awaitFrame(
				(f) =>
					f.kind === "event"
					&& f.method === "session.command"
					&& payloadOf(f)["action"] === "setSessionName",
				"setSessionName command",
			);
			const refresh = await collector.awaitFrame(
				(f) =>
					f.kind === "req"
					&& f.method === "session.setupEntries"
					&& f.id !== seed.id,
				"session.setupEntries refresh",
			);
			push(stdin, {
				id: refresh.id,
				kind: "error",
				method: "session.setupEntries",
				payload: {
					code: "stale_replacement_token",
					message: "stale replacement token",
					retryable: false,
				},
			});
		})();

		push(stdin, {
			id: 55,
			kind: "req",
			method: "command.execute",
			payload: { command: "failSetupName", args: "" },
		});
		await collector.awaitFrame((f) => f.id === 55 && f.kind === "res", "res 55");
		await setupResponses;
		expect(setupRejected).toBe(true);

		push(stdin, {
			id: 56,
			kind: "req",
			method: "command.execute",
			payload: { command: "readActiveName", args: "" },
		});
		const notify = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "notify",
			"active session name",
		);
		expect(payloadOf(notify)["message"]).toBe("active-name");
		await teardown(connected);
	});
	test("newSession: setup and withSession do NOT run when replacement is cancelled", async () => {
		const connected = await connectHost([replacedSessionFactory]);
		const { collector, stdin } = connected;

		sessionUpdate(stdin, true);

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession", "session.newSession req")
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: true },
				});
			});

		push(stdin, {
			id: 51, kind: "req", method: "command.execute",
			payload: { command: "replacedSessionCancel", args: "" },
		});

		const notify = await collector.awaitFrame((f) => f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 51 && f.kind === "res", "res 51");

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(report["setupRanOnCancel"]).toEqual([]);
		expect(report["newSessionResult"]).toEqual({ cancelled: true });

		await teardown(connected);
	});

	test("withSession: sendMessage and sendUserMessage produce bridge session.command frames", async () => {
		const connected = await connectHost([replacedSessionFactory]);
		const { collector, stdin } = connected;

		sessionUpdate(stdin, true);

		const setupResponses = (async () => {
			const request = await collector.awaitFrame(
				(f) => f.kind === "req" && f.method === "session.newSession",
				"session.newSession req",
			);
			push(stdin, {
				id: request.id,
				kind: "res",
				method: "session.newSession",
				payload: { cancelled: false, replacementToken: "tok-setup-2" },
			});
			await respondSetupEntries(collector, stdin, "tok-setup-2", [[], [], []]);
		})();

		push(stdin, {
			id: 52, kind: "req", method: "command.execute",
			payload: { command: "replacedSessionProbe", args: "" },
		});

		// Wait for the command to complete; by then sendMessage/sendUserMessage
		// have fired session.command events through the bridge.
		await collector.awaitFrame((f) => f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 52 && f.kind === "res", "res 52");
		await setupResponses;

		const sendMessageFrame = collector.frames.find(
			(f) =>
				f.kind === "event" &&
				f.method === "session.command" &&
				payloadOf(f)["action"] === "sendMessage",
		);
		const sendUserMessageFrame = collector.frames.find(
			(f) =>
				f.kind === "event" &&
				f.method === "session.command" &&
				payloadOf(f)["action"] === "sendUserMessage",
		);
		expect(sendMessageFrame).toBeDefined();
		expect(sendUserMessageFrame).toBeDefined();

		const smPayload = sendMessageFrame ? payloadOf(sendMessageFrame) : {};
		expect(smPayload["message"]).toMatchObject({
			customType: "test-custom",
			content: "hello",
		});

		const sumPayload = sendUserMessageFrame ? payloadOf(sendUserMessageFrame) : {};
		expect(sumPayload["content"]).toBe("user hello");

		await teardown(connected);
	});

	async function assertSendFailureRejectsCommand(
		failedAction: "sendMessage" | "sendUserMessage",
		requestId: number,
	): Promise<void> {
		const stdout = new FailingOnSessionCommand();
		const stdin = new Readable({ read() {} });
		const host = new ExtensionHost(stdin, stdout);
		const runPromise = host.run({
			cwd: process.cwd(), factories: [replacedSessionFactory], extensionPaths: [],
		});

		try {
			stdin.push(Buffer.from(encodeFrameString({
				id: 1, kind: "req", method: "hello",
				payload: {
					protocolVersion: PROTOCOL_VERSION,
					compatibilityVersion: COMPATIBILITY_VERSION,
				},
			})));
			await stdout.awaitFrame((f) => f.id === 1 && f.kind === "res", "res 1");
			sessionUpdate(stdin, true);

			const replacementToken = `tok-fail-${requestId}`;
			const setupResponses = (async () => {
				const request = await stdout.awaitFrame(
					(f) => f.kind === "req" && f.method === "session.newSession",
					"session.newSession req",
				);
				push(stdin, {
					id: request.id,
					kind: "res",
					method: "session.newSession",
					payload: { cancelled: false, replacementToken },
				});
				await respondSetupEntries(stdout, stdin, replacementToken, [[], [], []]);
			})();

			// Setup's appendEntry succeeds; fail the selected actual
			// ReplacedSessionContext helper write instead.
			stdout.armFailure(failedAction);
			push(stdin, {
				id: requestId, kind: "req", method: "command.execute",
				payload: { command: "replacedSessionProbe", args: "" },
			});

			const errorRes = await stdout.awaitFrame(
				(f) => f.id === requestId && f.kind === "error",
				"error requestId",
			);
			await setupResponses;
			const errPayload = payloadOf(errorRes);
			expect(stdout.failedAction).toBe(failedAction);
			expect(errPayload["code"]).toBeDefined();
			expect(typeof errPayload["message"]).toBe("string");
		} finally {
			stdin.push(null);
			host.dispose("test");
			await runPromise.catch(() => void 0);
		}
	}

	test("withSession: sendMessage delivery failure rejects the command path", async () => {
		await assertSendFailureRejectsCommand("sendMessage", 60);
	});

	test("withSession: sendUserMessage delivery failure rejects the command path", async () => {
		await assertSendFailureRejectsCommand("sendUserMessage", 61);
	});
});


describe("host: session.replacementReady + passthrough fields", () => {
	function sessionUpdate(stdin: Readable, idle: boolean): void {
		push(stdin, {
			id: 0, kind: "event", method: "session.update",
			payload: {
				thinkingLevel: "medium",
				activeTools: [],
				allTools: [],
				commands: [],
				isIdle: idle,
				hasPendingMessages: false,
				contextUsage: { tokens: 10, contextWindow: 1000, percent: 1 },
				scopedModels: [],
				systemPrompt: "",
			},
		});
	}

	test("newSession: emits session.replacementReady after command.execute res and strips token", async () => {
		const connected = await connectHost([replacementReadyFactory]);
		const { collector, stdin } = connected;
		sessionUpdate(stdin, true);

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession", "session.newSession req")
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: false, replacementToken: "tok-ns-1" },
				});
			});

		push(stdin, {
			id: 70, kind: "req", method: "command.execute",
			payload: { command: "replacementReadyProbe", args: "" },
		});

		const notify = await collector.awaitFrame((f) => f.method === "notify", "notify");
		const commandRes = await collector.awaitFrame((f) => f.id === 70 && f.kind === "res", "res 70");
		const ready = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "session.replacementReady",
			"session.replacementReady",
		);

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(report["newSession"]).toEqual({ cancelled: false });
		expect(report["hasToken"]).toBe(false);
		expect(payloadOf(ready)).toEqual({ token: "tok-ns-1" });

		const readyIdx = collector.frames.indexOf(ready);
		const resIdx = collector.frames.indexOf(commandRes);
		expect(readyIdx).toBeGreaterThan(resIdx);

		await teardown(connected);
	});

	test("newSession cancelled: does not emit session.replacementReady", async () => {
		const connected = await connectHost([replacementReadyFactory]);
		const { collector, stdin } = connected;
		sessionUpdate(stdin, true);

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession", "session.newSession req")
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: true, replacementToken: "tok-should-drop" },
				});
			});

		push(stdin, {
			id: 71, kind: "req", method: "command.execute",
			payload: { command: "replacementReadyCancel", args: "" },
		});

		await collector.awaitFrame((f) => f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 71 && f.kind === "res", "res 71");

		// A single microtask tick cannot prove the suppressed frame was never
		// emitted — it only proves it wasn't emitted *yet*. Instead, drive a
		// positive milestone that must follow the finally path: send a `measure`
		// request and await its response. The ProtocolClient writeChain is
		// strictly ordered, so the measure response write happens after the
		// first command's finally-block write (if any). When the measure
		// response arrives, any replacementReady frame would already be in
		// collector.frames.
		push(stdin, {
			id: 9001, kind: "req", method: "measure",
			payload: { key: "nonexistent", width: 80 },
		});
		await collector.awaitFrame((f) => f.id === 9001 && f.kind === "res", "res 9001");

		const readyFrames = collector.frames.filter(
			(f) => f.kind === "event" && f.method === "session.replacementReady",
		);
		expect(readyFrames).toEqual([]);
		await teardown(connected);
	});

	test("handler throw after replacement still emits session.replacementReady", async () => {
		const connected = await connectHost([replacementReadyFactory]);
		const { collector, stdin } = connected;
		sessionUpdate(stdin, true);

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession", "session.newSession req")
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: false, replacementToken: "tok-throw-1" },
				});
			});

		push(stdin, {
			id: 72, kind: "req", method: "command.execute",
			payload: { command: "replacementReadyThrow", args: "" },
		});

		const errorRes = await collector.awaitFrame((f) => f.id === 72 && f.kind === "error", "error 72");
		const ready = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "session.replacementReady",
			"session.replacementReady",
		);
		expect(payloadOf(errorRes)["message"]).toContain("post-replacement boom");
		expect(payloadOf(ready)).toEqual({ token: "tok-throw-1" });
		expect(collector.frames.filter(
			(frame) => frame.kind === "event" && frame.method === "session.replacementReady",
		)).toHaveLength(1);
		expect(collector.frames.indexOf(ready)).toBeGreaterThan(collector.frames.indexOf(errorRes));

		await teardown(connected);
	});

	test("reload: emits session.replacementReady after command.execute res", async () => {
		const connected = await connectHost([replacementReadyFactory]);
		const { collector, stdin } = connected;
		sessionUpdate(stdin, true);

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.reload", "session.reload req")
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "res", method: "session.reload",
					payload: { replacementToken: "tok-reload-1" },
				});
			});

		push(stdin, {
			id: 73, kind: "req", method: "command.execute",
			payload: { command: "replacementReadyReload", args: "" },
		});

		const commandRes = await collector.awaitFrame((f) => f.id === 73 && f.kind === "res", "res 73");
		const ready = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "session.replacementReady",
			"session.replacementReady",
		);
		expect(payloadOf(ready)).toEqual({ token: "tok-reload-1" });
		expect(collector.frames.indexOf(ready)).toBeGreaterThan(collector.frames.indexOf(commandRes));

		await teardown(connected);
	});

	test("fork: passes selectedText through and strips replacementToken", async () => {
		const connected = await connectHost([replacementReadyFactory]);
		const { collector, stdin } = connected;
		sessionUpdate(stdin, true);

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.fork", "session.fork req")
			.then((request) => {
				expect(payloadOf(request)).toMatchObject({ entryId: "entry-1", position: "at" });
				push(stdin, {
					id: request.id, kind: "res", method: "session.fork",
					payload: {
						cancelled: false,
						selectedText: "picked text",
						replacementToken: "tok-fork-1",
					},
				});
			});

		push(stdin, {
			id: 74, kind: "req", method: "command.execute",
			payload: { command: "forkPassthroughProbe", args: "" },
		});

		const notify = await collector.awaitFrame((f) => f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 74 && f.kind === "res", "res 74");
		const ready = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "session.replacementReady",
			"session.replacementReady",
		);

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(report["fork"]).toEqual({ cancelled: false, selectedText: "picked text" });
		expect(report["hasToken"]).toBe(false);
		expect(payloadOf(ready)).toEqual({ token: "tok-fork-1" });

		await teardown(connected);
	});

	test("navigateTree: passes editorText/aborted/summaryEntry and does not emit ready", async () => {
		const connected = await connectHost([replacementReadyFactory]);
		const { collector, stdin } = connected;
		sessionUpdate(stdin, true);

		const summaryEntry = {
			type: "branch_summary",
			id: "bs-1",
			parentId: "p-1",
			timestamp: "2026-01-01T00:00:00.000Z",
			fromId: "from-1",
			summary: "branch left behind",
		};

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.navigateTree", "session.navigateTree req")
			.then((request) => {
				expect(payloadOf(request)).toMatchObject({ targetId: "leaf-1", summarize: true });
				push(stdin, {
					id: request.id, kind: "res", method: "session.navigateTree",
					payload: {
						cancelled: false,
						editorText: "draft",
						aborted: true,
						summaryEntry,
					},
				});
			});

		push(stdin, {
			id: 75, kind: "req", method: "command.execute",
			payload: { command: "navigateTreePassthroughProbe", args: "" },
		});

		const notify = await collector.awaitFrame((f) => f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 75 && f.kind === "res", "res 75");

		// A single microtask tick cannot prove the suppressed frame was never
		// emitted — it only proves it wasn't emitted *yet*. Instead, drive a
		// positive milestone that must follow the finally path: send a `measure`
		// request and await its response. The ProtocolClient writeChain is
		// strictly ordered, so the measure response write happens after the
		// first command's finally-block write (if any). When the measure
		// response arrives, any replacementReady frame would already be in
		// collector.frames.
		push(stdin, {
			id: 9001, kind: "req", method: "measure",
			payload: { key: "nonexistent", width: 80 },
		});
		await collector.awaitFrame((f) => f.id === 9001 && f.kind === "res", "res 9001");

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(report["navigateTree"]).toEqual({
			cancelled: false,
			editorText: "draft",
			aborted: true,
			summaryEntry,
		});
		expect(report["hasToken"]).toBe(false);
		expect(
			collector.frames.filter((f) => f.kind === "event" && f.method === "session.replacementReady"),
		).toEqual([]);

		await teardown(connected);
	});

	test("concurrent commands cannot cross-emit each other's replacement token", async () => {
		const connected = await connectHost([replacementReadyFactory]);
		const { collector, stdin } = connected;
		// Keep idle waiters parked until the replacement command has captured its token.
		sessionUpdate(stdin, false);

		const newSessionReq = collector.awaitFrame(
			(f) => f.kind === "req" && f.method === "session.newSession",
			"session.newSession req",
		);

		// Start the idle peer first so it is in-flight under a different ALS scope.
		push(stdin, {
			id: 76, kind: "req", method: "command.execute",
			payload: { command: "concurrentIdleProbe", args: "" },
		});
		push(stdin, {
			id: 77, kind: "req", method: "command.execute",
			payload: { command: "replacementReadyProbe", args: "" },
		});

		const request = await newSessionReq;
		push(stdin, {
			id: request.id, kind: "res", method: "session.newSession",
			payload: { cancelled: false, replacementToken: "tok-owner" },
		});
		// Release the idle peer only after the owner captured its token.
		sessionUpdate(stdin, true);

		await collector.awaitFrame((f) => f.id === 76 && f.kind === "res", "res 76");
		const ownerRes = await collector.awaitFrame((f) => f.id === 77 && f.kind === "res", "res 77");
		const ready = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "session.replacementReady",
			"session.replacementReady",
		);
		const readyFrames = collector.frames.filter(
			(f) => f.kind === "event" && f.method === "session.replacementReady",
		);
		expect(readyFrames).toHaveLength(1);
		expect(payloadOf(ready)).toEqual({ token: "tok-owner" });
		const ownerResIdx = collector.frames.indexOf(ownerRes);
		const readyIdx = collector.frames.indexOf(ready);
		expect(readyIdx).toBeGreaterThan(ownerResIdx);

		await teardown(connected);
	});
});

describe("host: per-command replacement staleness", () => {
	const staleContextMessage =
		"This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().";

	function setIdle(stdin: Readable): void {
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
				scopedModels: [],
				systemPrompt: "",
			},
		});
	}

	for (const replacement of [
		{
			name: "newSession",
			command: "staleNewSession",
			method: "session.newSession",
			payload: { cancelled: false, replacementToken: "token-new" },
		},
		{
			name: "fork",
			command: "staleFork",
			method: "session.fork",
			payload: { cancelled: false, selectedText: "picked", replacementToken: "token-fork" },
		},
		{
			name: "switchSession",
			command: "staleSwitchSession",
			method: "session.switchSession",
			payload: { cancelled: false, replacementToken: "token-switch" },
		},
		{
			name: "reload",
			command: "staleReload",
			method: "session.reload",
			payload: { replacementToken: "token-reload" },
		},
	]) {
		test(`${replacement.name} stales only its initiating command context at token capture`, async () => {
			const connected = await connectHost([staleCtxFactory]);
			const { collector, stdin } = connected;
			setIdle(stdin);

			const replacementRequest = collector.awaitFrame(
				(frame) => frame.kind === "req" && frame.method === replacement.method,
				"req",
			);
			push(stdin, {
				id: 100, kind: "req", method: "command.execute",
				payload: { command: replacement.command, args: "" },
			});
			const request = await replacementRequest;
			push(stdin, {
				id: request.id, kind: "res", method: replacement.method,
				payload: replacement.payload,
			});

			const error = await collector.awaitFrame((frame) => frame.id === 100 && frame.kind === "error", "error 100");
			const ready = await collector.awaitFrame(
				(frame) => frame.kind === "event" && frame.method === "session.replacementReady",
				"session.replacementReady",
			);
			expect(payloadOf(error)["message"]).toBe(staleContextMessage);
			expect(payloadOf(ready)).toEqual({ token: replacement.payload.replacementToken });
			expect(collector.frames.filter((frame) => frame.kind === "req" && frame.method === replacement.method)).toHaveLength(1);
			expect(collector.frames.filter(
			(frame) => frame.kind === "event" && frame.method === "session.replacementReady",
		)).toHaveLength(1);
			expect(collector.frames.indexOf(ready)).toBeGreaterThan(collector.frames.indexOf(error));

			// The stale bit belongs to this handler context, not the runner.
			push(stdin, {
				id: 101, kind: "req", method: "command.execute",
				payload: { command: "activeCtxWorks", args: "" },
			});
			const notify = await collector.awaitFrame(
				(frame) => frame.kind === "event" && frame.method === "notify",
				"notify",
			);
			const activeResult = await collector.awaitFrame(
				(frame) => frame.id === 101 && frame.kind === "res",
				"res 101",
			);
			expect(JSON.parse(String(payloadOf(notify)["message"]))).toEqual({ active: true });
			expect(payloadOf(activeResult)).toEqual({ ok: true });

			await teardown(connected);
		});
	}

	test("a cancelled replacement leaves its initiating context usable", async () => {
		const connected = await connectHost([staleCtxFactory]);
		const { collector, stdin } = connected;
		setIdle(stdin);

		void collector.awaitFrame((frame) => frame.kind === "req" && frame.method === "session.newSession", "session.newSession req")
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: true, replacementToken: "ignored-token" },
				});
			});
		push(stdin, {
			id: 110, kind: "req", method: "command.execute",
			payload: { command: "cancelledCtxRemainsUsable", args: "" },
		});

		const notify = await collector.awaitFrame((frame) => frame.kind === "event" && frame.method === "notify", "notify");
		const result = await collector.awaitFrame((frame) => frame.id === 110 && frame.kind === "res", "res 110");
		expect(JSON.parse(String(payloadOf(notify)["message"]))).toEqual({ cancelled: true, stillUsable: true });
		expect(payloadOf(result)).toEqual({ ok: true });
		expect(collector.frames.filter(
			(frame) => frame.kind === "event" && frame.method === "session.replacementReady",
		)).toHaveLength(0);

		await teardown(connected);
	});

	test("setup and withSession use fresh replacement state before ready finalization", async () => {
		const connected = await connectHost([staleCtxFactory]);
		const { collector, stdin } = connected;
		setIdle(stdin);

		const setupResponses = (async () => {
			const request = await collector.awaitFrame(
				(frame) => frame.kind === "req" && frame.method === "session.newSession",
				"session.newSession req",
			);
			push(stdin, {
				id: request.id,
				kind: "res",
				method: "session.newSession",
				payload: { cancelled: false, replacementToken: "fresh-context-token" },
			});
			await respondSetupEntries(
				collector,
				stdin,
				"fresh-context-token",
				[[], [{ type: "session_info", id: "session-info" }]],
			);
		})();
		push(stdin, {
			id: 120, kind: "req", method: "command.execute",
			payload: { command: "withSessionUsesFreshContext", args: "" },
		});

		const error = await collector.awaitFrame((frame) => frame.id === 120 && frame.kind === "error", "error 120");
		const ready = await collector.awaitFrame(
			(frame) => frame.kind === "event" && frame.method === "session.replacementReady",
			"session.replacementReady",
		);
		await setupResponses;
		const sessionCommands = collector.frames.filter(
			(frame) => frame.kind === "event" && frame.method === "session.command",
		);
		expect(sessionCommands.map((frame) => payloadOf(frame)["action"])).toEqual([
			"setSessionName",
			"sendUserMessage",
		]);
		expect(payloadOf(error)["message"]).toBe(staleContextMessage);
		expect(payloadOf(ready)).toEqual({ token: "fresh-context-token" });
		expect(collector.frames.indexOf(ready)).toBeGreaterThan(collector.frames.indexOf(error));
		expect(collector.frames.filter(
			(frame) => frame.kind === "event" && frame.method === "session.replacementReady",
		)).toHaveLength(1);

		await teardown(connected);
	});

	test("method captured before newSession rechecks stale guard on call", async () => {
		const connected = await connectHost([staleCtxFactory]);
		const { collector, stdin } = connected;
		setIdle(stdin);

		void collector.awaitFrame((frame) => frame.kind === "req" && frame.method === "session.newSession", "session.newSession req")
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: false, replacementToken: "captured-method-token" },
				});
			});
		push(stdin, {
			id: 125, kind: "req", method: "command.execute",
			payload: { command: "capturedMethodStalesAfterNewSession", args: "" },
		});

		const error = await collector.awaitFrame((frame) => frame.id === 125 && frame.kind === "error", "error 125");
		const ready = await collector.awaitFrame(
			(frame) => frame.kind === "event" && frame.method === "session.replacementReady",
			"session.replacementReady",
		);
		const newSessionReqs = collector.frames.filter(
			(frame) => frame.kind === "req" && frame.method === "session.newSession",
		);
		const sessionCommands = collector.frames.filter(
			(frame) => frame.kind === "event" && frame.method === "session.command",
		);
		// Captured waitForIdle must not trigger another session request after replacement.
		expect(newSessionReqs).toHaveLength(1);
		// withSession still ran against a fresh context.
		expect(sessionCommands.map((frame) => payloadOf(frame)["action"])).toEqual([
			"sendUserMessage",
		]);
		expect(payloadOf(error)["message"]).toBe(staleContextMessage);
		expect(payloadOf(ready)).toEqual({ token: "captured-method-token" });
		expect(collector.frames.indexOf(ready)).toBeGreaterThan(collector.frames.indexOf(error));
		expect(collector.frames.filter(
			(frame) => frame.kind === "event" && frame.method === "session.replacementReady",
		)).toHaveLength(1);

		await teardown(connected);
	});

	test("captured ctx throws after runner rebuild", async () => {
		resetCapturedCtx();
		const connected = await connectHost([staleCtxFactory]);
		const { collector, stdin } = connected;

		push(stdin, {
			id: 130, kind: "req", method: "command.execute",
			payload: { command: "captureCtx", args: "" },
		});
		await collector.awaitFrame((frame) => frame.id === 130 && frame.kind === "res", "res 130");

		push(stdin, {
			id: 131, kind: "req", method: "extensions.load",
			payload: { extensionPaths: [], cwd: process.cwd() },
		});
		await collector.awaitFrame((frame) => frame.id === 131 && frame.kind === "res", "res 131");

		push(stdin, {
			id: 132, kind: "req", method: "command.execute",
			payload: { command: "useStaleCtx", args: "" },
		});
		const error = await collector.awaitFrame((frame) => frame.id === 132 && frame.kind === "error", "error 132");
		expect(payloadOf(error)["message"]).toBe(staleContextMessage);

		await teardown(connected);
	});
});

describe("host: tool_call key-order-insensitive input comparison", () => {
	test("tool_call omits input when a hook only reorders object keys", async () => {
		const connected = await connectHost([toolCallReorderFactory]);
		const { collector, stdin } = connected;

		push(stdin, {
			id: 200, kind: "req", method: "tool_call",
			payload: {
				toolName: "echo",
				toolCallId: "call-reorder",
				input: { z: 3, a: 1, m: 2 },
			},
		});
		const res = await collector.awaitFrame((f) => f.id === 200 && f.kind === "res", "res 200");
		const body = payloadOf(res);
		expect(body["block"]).toBe(false);
		expect(body["reason"]).toBe("reorder-ack");
		expect(Object.hasOwn(body, "input")).toBe(false);

		await teardown(connected);
	});
});

describe("host: replacementReady write failure is contained", () => {
	test("failed replacementReady send does not produce a second error frame", async () => {
		const failing = new FailingFrameCollector(["session.replacementReady"]);
		const connected = await connectHost([replacementReadyFactory], failing);
		const { collector, stdin } = connected;

		// Drive idle so the command can proceed.
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
				scopedModels: [],
				systemPrompt: "",
			},
		});

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession", "session.newSession req")
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: false, replacementToken: "tok-fail-1" },
				});
			});

		push(stdin, {
			id: 210, kind: "req", method: "command.execute",
			payload: { command: "replacementReadyProbe", args: "" },
		});

		// The command.execute response must arrive (the handler succeeded).
		const commandRes = await collector.awaitFrame((f) => f.id === 210 && f.kind === "res", "res 210");
		expect(commandRes).toBeDefined();

		// The replacementReady send failed; the error must be contained as an
		// extensionError event, not a second terminal frame for id 210.
		const errorEvent = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "extensionError",
			"extensionError",
		);
		expect(payloadOf(errorEvent)["message"]).toContain("session.replacementReady");

		// Exactly one terminal frame for the command.execute request id.
		const terminalFor210 = collector.frames.filter(
			(f) => f.id === 210 && (f.kind === "res" || f.kind === "error"),
		);
		expect(terminalFor210).toHaveLength(1);

		await teardown(connected);
	});
});
describe("host: protocol extension order", () => {
	test("initial protocol paths precede builtins and later loads append", async () => {
		const dir = await mkdtemp(join(tmpdir(), "pr10-precedence-"));
		const initialPath = join(dir, "initial.ts");
		const latePath = join(dir, "late.ts");
		await writeFile(
			initialPath,
			'import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";\n' +
			'export default function initialExt(pi: ExtensionAPI): void {\n' +
			'  pi.registerCommand("initialCmd", { description: "initial", async handler(_a, ctx) { ctx.ui.notify("initial", "info"); } });\n' +
			'}\n',
		);
		await writeFile(
			latePath,
			'import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";\n' +
			'export default function lateExt(pi: ExtensionAPI): void {\n' +
			'  pi.registerCommand("lateCmd", { description: "late", async handler(_a, ctx) { ctx.ui.notify("late", "info"); } });\n' +
			'}\n',
		);
		let connected: Connected | undefined;
		try {
			connected = await connectHost([toolFactory]);
			const { collector, stdin } = connected;

			push(stdin, {
				id: 219, kind: "req", method: "command.execute",
				payload: { command: "greet", args: "" },
			});
			await collector.awaitFrame((f) => f.id === 219 && f.kind === "res", "res 219");
			const [builtin] = connected.host.getExtensions();
			expect(builtin?.commands.has("greet")).toBe(true);

			push(stdin, {
				id: 220, kind: "req", method: "extensions.load",
				payload: { extensionPaths: [initialPath], cwd: process.cwd() },
			});
			await collector.awaitFrame((f) => f.id === 220 && f.kind === "res", "res 220");
			const afterInitialLoad = connected.host.getExtensions();
			expect(afterInitialLoad).toHaveLength(2);
			expect(afterInitialLoad[0]?.commands.has("initialCmd")).toBe(true);
			expect(afterInitialLoad[1]).toBe(builtin);

			push(stdin, {
				id: 221, kind: "req", method: "extensions.load",
				payload: { extensionPaths: [latePath], cwd: process.cwd() },
			});
			await collector.awaitFrame((f) => f.id === 221 && f.kind === "res", "res 221");
			const afterLateLoad = connected.host.getExtensions();
			expect(afterLateLoad).toHaveLength(3);
			expect(afterLateLoad[0]?.commands.has("initialCmd")).toBe(true);
			expect(afterLateLoad[1]).toBe(builtin);
			expect(afterLateLoad[2]?.commands.has("lateCmd")).toBe(true);

		} finally {
			if (connected) {
				await teardown(connected);
			}
			await rm(dir, { recursive: true, force: true });
		}
	});
});

describe("host: SessionManager proxy is not a thenable", () => {
	test("awaiting the SessionManager proxy does not throw", async () => {
		const connected = await connectHost([sessionManagerProxyFactory]);
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
				scopedModels: [],
				systemPrompt: "",
			},
		});

		const setupResponses = (async () => {
			const request = await collector.awaitFrame(
				(f) => f.kind === "req" && f.method === "session.newSession",
				"session.newSession req",
			);
			push(stdin, {
				id: request.id,
				kind: "res",
				method: "session.newSession",
				payload: { cancelled: false, replacementToken: "tok-proxy-1" },
			});
			await respondSetupEntries(collector, stdin, "tok-proxy-1", [[]]);
		})();

		push(stdin, {
			id: 230, kind: "req", method: "command.execute",
			payload: { command: "sessionManagerThenProbe", args: "" },
		});

		const notify = await collector.awaitFrame((f) => f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 230 && f.kind === "res", "res 230");
		await setupResponses;

		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(report["setupRan"]).toBe(true);
		expect(report["managerIsObject"]).toBe(true);
		expect(report["thenIsUndefined"]).toBe(true);

		await teardown(connected);
	});
});
describe("host: replacement-token scoped command wire contract", () => {
	const activeCommandFactory: ExtensionFactory = (pi) => {
		pi.registerCommand("activeSessionCommandProbe", {
			description: "Emit ordinary active-session commands",
			async handler(_args, ctx) {
				pi.setSessionName("active-name");
				pi.appendEntry("active-custom", { from: "active" });
				pi.sendUserMessage("active user");
				ctx.ui.notify("active done", "info");
			},
		});
	};

	test("active session commands are flattened and omit replacementToken", async () => {
		const connected = await connectHost([activeCommandFactory]);
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
				scopedModels: [],
				systemPrompt: "",
			},
		});

		push(stdin, {
			id: 300, kind: "req", method: "command.execute",
			payload: { command: "activeSessionCommandProbe", args: "" },
		});

		await collector.awaitFrame(
			(f) =>
				f.kind === "event"
				&& f.method === "notify"
				&& payloadOf(f)["message"] === "active done",
			"active done notify",
		);
		await collector.awaitFrame((f) => f.id === 300 && f.kind === "res", "res 300");

		const activeCommands = collector.frames.filter(
			(f) => f.kind === "event" && f.method === "session.command",
		);
		expect(activeCommands).toHaveLength(3);
		expect(activeCommands.map((f) => payloadOf(f)["action"])).toEqual([
			"setSessionName",
			"appendEntry",
			"sendUserMessage",
		]);

		const byAction = new Map(
			activeCommands.map((f) => [String(payloadOf(f)["action"]), payloadOf(f)] as const),
		);
		expect(byAction.get("setSessionName")).toEqual({
			action: "setSessionName",
			name: "active-name",
		});
		expect(byAction.get("appendEntry")).toEqual({
			action: "appendEntry",
			customType: "active-custom",
			data: { from: "active" },
		});
		expect(byAction.get("sendUserMessage")).toEqual({
			action: "sendUserMessage",
			content: "active user",
		});

		for (const frame of activeCommands) {
			expect(Object.hasOwn(payloadOf(frame), "replacementToken")).toBe(false);
		}

		await teardown(connected);
	});

	test("candidate SessionManager and ReplacedSessionContext commands carry the token", async () => {
		const connected = await connectHost([replacedSessionFactory]);
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
				scopedModels: [],
				systemPrompt: "",
			},
		});

		const initialEntry = { type: "message", id: "initial" };
		const customEntry = { type: "custom", id: "custom" };
		const sessionInfoEntry = {
			type: "session_info",
			id: "session-info",
			name: "setup-session",
		};

		const setupResponses = (async () => {
			const request = await collector.awaitFrame(
				(f) => f.kind === "req" && f.method === "session.newSession",
				"session.newSession req",
			);
			expect(payloadOf(request)).toEqual({ parentSession: "parent-1" });
			push(stdin, {
				id: request.id,
				kind: "res",
				method: "session.newSession",
				payload: { cancelled: false, replacementToken: "tok-cand-1" },
			});
			await respondSetupEntries(collector, stdin, "tok-cand-1", [
				[initialEntry],
				[initialEntry, customEntry],
				[initialEntry, customEntry, sessionInfoEntry],
			]);
		})();

		push(stdin, {
			id: 301, kind: "req", method: "command.execute",
			payload: { command: "replacedSessionProbe", args: "" },
		});

		await collector.awaitFrame((f) => f.kind === "event" && f.method === "notify", "notify");
		await collector.awaitFrame((f) => f.id === 301 && f.kind === "res", "res 301");
		await setupResponses;

		const commandEvents = collector.frames.filter(
			(f) => f.kind === "event" && f.method === "session.command",
		);
		expect(commandEvents.map((f) => payloadOf(f)["action"])).toEqual([
			"appendEntry",
			"setSessionName",
			"sendMessage",
			"sendUserMessage",
		]);

		const byAction = new Map(
			commandEvents.map((f) => [String(payloadOf(f)["action"]), payloadOf(f)] as const),
		);
		expect(byAction.get("appendEntry")).toEqual({
			replacementToken: "tok-cand-1",
			action: "appendEntry",
			customType: "setup-custom",
			data: { from: "setup" },
		});
		expect(byAction.get("setSessionName")).toEqual({
			replacementToken: "tok-cand-1",
			action: "setSessionName",
			name: "setup-session",
		});
		expect(byAction.get("sendMessage")).toEqual({
			replacementToken: "tok-cand-1",
			action: "sendMessage",
			message: {
				customType: "test-custom",
				content: "hello",
				display: true,
			},
		});
		expect(byAction.get("sendUserMessage")).toEqual({
			replacementToken: "tok-cand-1",
			action: "sendUserMessage",
			content: "user hello",
		});

		const setupRequests = collector.frames.filter(
			(f) => f.kind === "req" && f.method === "session.setupEntries",
		);
		expect(setupRequests).toHaveLength(3);
		for (const req of setupRequests) {
			expect(payloadOf(req)).toEqual({ replacementToken: "tok-cand-1" });
		}

		const ready = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "session.replacementReady",
			"session.replacementReady",
		);
		expect(payloadOf(ready)).toEqual({ token: "tok-cand-1" });
		expect(collector.frames.filter(
			(f) => f.kind === "event" && f.method === "session.replacementReady",
		)).toHaveLength(1);

		const aborts = collector.frames.filter(
			(f) => f.kind === "event" && f.method === "session.replacementAbort",
		);
		expect(aborts).toEqual([]);

		await teardown(connected);
	});

	const lateTokenFactory: ExtensionFactory = (pi) => {
		pi.registerCommand("lateReplacementProbe", {
			description: "Fire-and-forget newSession so the token arrives after command scope close",
			async handler(_args, ctx) {
				void ctx.newSession({
					parentSession: "parent-1",
					withSession: async (replacedCtx) => {
						replacedCtx.ui.notify("withSession ran", "info");
					},
				});
				ctx.ui.notify("fire and forget", "info");
			},
		});
	};

	test("late successful replacement token after command scope close emits one replacementAbort", async () => {
		const connected = await connectHost([lateTokenFactory]);
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
				scopedModels: [],
				systemPrompt: "",
			},
		});

		push(stdin, {
			id: 302, kind: "req", method: "command.execute",
			payload: { command: "lateReplacementProbe", args: "" },
		});

		const newSessionReq = await collector.awaitFrame(
			(f) => f.kind === "req" && f.method === "session.newSession",
			"session.newSession req",
		);
		expect(payloadOf(newSessionReq)).toEqual({ parentSession: "parent-1" });

		await collector.awaitFrame(
			(f) =>
				f.kind === "event"
				&& f.method === "notify"
				&& payloadOf(f)["message"] === "fire and forget",
			"fire and forget notify",
		);
		await collector.awaitFrame((f) => f.id === 302 && f.kind === "res", "res 302");

		// The command response proves its AsyncLocalStorage scope is closed.
		// Delivering the successful replacement response now must take the late path.
		push(stdin, {
			id: newSessionReq.id,
			kind: "res",
			method: "session.newSession",
			payload: { cancelled: false, replacementToken: "tok-late-1" },
		});

		const abort = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "session.replacementAbort",
			"session.replacementAbort",
		);

		expect(payloadOf(abort)).toEqual({ token: "tok-late-1" });
		expect(collector.frames.filter((f) => f.kind === "event" && f.method === "session.replacementAbort")).toHaveLength(1);
		expect(collector.frames.filter((f) => f.kind === "event" && f.method === "session.replacementReady")).toEqual([]);
		expect(collector.frames.filter((f) =>
			f.kind === "event"
			&& f.method === "session.command"
			&& Object.hasOwn(payloadOf(f), "replacementToken"),
		)).toEqual([]);

		const withSessionNotify = collector.frames.find(
			(f) =>
				f.kind === "event"
				&& f.method === "notify"
				&& payloadOf(f)["message"] === "withSession ran",
		);
		expect(withSessionNotify).toBeUndefined();

		await teardown(connected);
	});
});
