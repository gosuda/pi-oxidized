/**
 * Lean Mode-2 tests: protocol-only hello, extensions.load registry,
 * prepare/validate/execute/cancel, commands, flags, shortcuts, providers,
 * lifecycle hooks, per-extension load-error isolation — plus the structural
 * and load-time proofs that lean selection never evaluates host.ts,
 * builtins, virtual-modules, or the upstream package graph.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { Readable } from "node:stream";
import {
	COMPATIBILITY_VERSION,
	type Frame,
	FrameDecoder,
	PROTOCOL_VERSION,
	encodeFrameString,
} from "@earendil-works/pi-tui-protocol";
import { parseLeanExtension } from "../src/lean-api.ts";
import {
	findExcludedImport,
	LeanRunner,
	parseStreamingJson,
} from "../src/lean-runner.ts";

const PACKAGE_DIR = resolve(import.meta.dirname, "..");
const LEAN_FIXTURES = resolve(import.meta.dirname, "fixtures", "lean");
const ECHO_ENTRY = join(LEAN_FIXTURES, "echo.mjs");
const FORBIDDEN_ENTRY = join(LEAN_FIXTURES, "forbidden-import.mjs");
const FACTORY_ENTRY = join(LEAN_FIXTURES, "factory-surface.mjs");
const FOLD_FIRST_ENTRY = join(LEAN_FIXTURES, "fold-first.mjs");
const FOLD_SECOND_ENTRY = join(LEAN_FIXTURES, "fold-second.mjs");
const PRELOAD = resolve(import.meta.dirname, "fixtures", "lean-forbid-compat-graph.ts");

type Marker = { name: string; value: unknown };

function markerLog(): Marker[] {
	const key = "__leanEchoLog";
	const holder = globalThis as Record<string, unknown>;
	const log = holder[key];
	return Array.isArray(log) ? (log as Marker[]) : [];
}

afterEach(() => {
	(globalThis as Record<string, unknown>).__leanEchoLog = [];
});

// ---------------------------------------------------------------------------
// In-process JSONL link
// ---------------------------------------------------------------------------

/** Collecting writable + request driver for one LeanRunner instance. */
class LeanLink {
	readonly runner: LeanRunner;
	private readonly stdin = new Readable({ read() {} });
	private readonly decoder = new FrameDecoder();
	private readonly frames: Frame[] = [];
	private readonly waiters: Array<{
		predicate: (frame: Frame) => boolean;
		resolve: (frame: Frame) => void;
		reject: (error: Error) => void;
		timer: ReturnType<typeof setTimeout>;
	}> = [];
	readonly runPromise: Promise<void>;

	constructor(options: { cwd: string; extensionPaths: string[] }) {
		this.runner = new LeanRunner(this.stdin, {
			write: (chunk: Uint8Array) => {
				for (const frame of this.decoder.push(chunk)) {
					this.deliver(frame);
				}
			},
		});
		this.runPromise = this.runner.run(options);
	}

	private deliver(frame: Frame): void {
		this.frames.push(frame);
		for (let index = this.waiters.length - 1; index >= 0; index--) {
			const waiter = this.waiters[index];
			if (waiter !== undefined && waiter.predicate(frame)) {
				this.waiters.splice(index, 1);
				clearTimeout(waiter.timer);
				waiter.resolve(frame);
			}
		}
	}

	send(frame: Frame): void {
		this.stdin.push(Buffer.from(encodeFrameString(frame)));
	}

	request(id: number, method: string, payload: unknown = {}): void {
		this.send({ id, kind: "req", method, payload });
	}

	event(method: string, payload: unknown = {}): void {
		this.send({ id: 0, kind: "event", method, payload });
	}

	waitFor(predicate: (frame: Frame) => boolean, timeoutMs = 5000): Promise<Frame> {
		const existing = this.frames.find(predicate);
		if (existing !== undefined) return Promise.resolve(existing);
		const { promise, resolve: resolveWait, reject } = Promise.withResolvers<Frame>();
		const timer = setTimeout(() => {
			const index = this.waiters.findIndex((w) => w.resolve === resolveWait);
			if (index !== -1) this.waiters.splice(index, 1);
			reject(new Error("timed out waiting for frame"));
		}, timeoutMs);
		this.waiters.push({ predicate, resolve: resolveWait, reject, timer });
		return promise;
	}

	response(id: number, method: string): Promise<Frame> {
		return this.waitFor((f) => f.id === id && f.kind === "res" && f.method === method);
	}

	error(id: number, method: string): Promise<Frame> {
		return this.waitFor((f) => f.id === id && f.kind === "error" && f.method === method);
	}

	async hello(id: number, compatibilityVersion: string = COMPATIBILITY_VERSION): Promise<Frame> {
		this.request(id, "hello", {
			protocolVersion: PROTOCOL_VERSION,
			compatibilityVersion,
		});
		const ack = await this.response(id, "hello");
		return ack;
	}

	async finish(): Promise<void> {
		this.stdin.push(null);
		await this.runPromise.catch(() => void 0);
	}
}

function payload(frame: Frame): Record<string, unknown> {
	return frame.payload as Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// Hello handshake (protocol-only)
// ---------------------------------------------------------------------------

describe("lean: hello handshake", () => {
	test("matching protocolVersion acks even with a foreign compatibilityVersion", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		const ack = await link.hello(1, "0.99.0-not-the-compat-version");
		expect(payload(ack)["protocolVersion"]).toBe(PROTOCOL_VERSION);
		expect(link.runner.isDisposed).toBe(false);
		await link.finish();
	});

	test("missing compatibilityVersion is ignored entirely", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		link.send({
			id: 1,
			kind: "req",
			method: "hello",
			payload: { protocolVersion: PROTOCOL_VERSION },
		});
		const ack = await link.response(1, "hello");
		expect(payload(ack)["protocolVersion"]).toBe(PROTOCOL_VERSION);
		await link.finish();
	});

	test("protocol version mismatch terminates the runner", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		link.request(1, "hello", {
			protocolVersion: 999,
			compatibilityVersion: COMPATIBILITY_VERSION,
		});
		await link.finish();
		expect(link.runner.isDisposed).toBe(true);
	});
});

// ---------------------------------------------------------------------------
// extensions.load registry snapshot + per-extension load-error isolation
// ---------------------------------------------------------------------------

describe("lean: extensions.load registry", () => {
	test("snapshot is RegistrySnapshotWire-shaped; bad entries are isolated", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);

		link.request(2, "extensions.load", {
			extensionPaths: [
				ECHO_ENTRY,
				FORBIDDEN_ENTRY,
				FACTORY_ENTRY,
				join(LEAN_FIXTURES, "does-not-exist.mjs"),
				join(LEAN_FIXTURES, "entry.ts"),
			],
			cwd: PACKAGE_DIR,
		});
		const res = payload(await link.response(2, "extensions.load"));

		expect(res["extensions"]).toBe(1);
		const errors = res["errors"] as Array<{ path: string; error: string }>;
		expect(errors).toHaveLength(4);
		const byPath = new Map(errors.map((e) => [e.path, e.error]));
		expect(byPath.get(FORBIDDEN_ENTRY)).toContain("excluded import");
		expect(byPath.get(FACTORY_ENTRY)).toContain("declarative lean extension object");
		expect(byPath.get(join(LEAN_FIXTURES, "does-not-exist.mjs"))).toBeDefined();
		expect(byPath.get(join(LEAN_FIXTURES, "entry.ts"))).toContain("prebundled .mjs");

		const tools = res["tools"] as Array<Record<string, unknown>>;
		expect(tools.map((t) => t["name"])).toEqual(["echo", "slow"]);
		const echo = tools[0] as Record<string, unknown>;
		expect(echo["label"]).toBe("Echo");
		expect(echo["description"]).toBe("Echo the input text back");
		expect((echo["parameters"] as Record<string, unknown>)["type"]).toBe("object");

		const commands = res["commands"] as Array<Record<string, unknown>>;
		expect(commands).toEqual([
			{ name: "greet", description: "Record a greeting", source: ECHO_ENTRY },
		]);

		const flags = res["flags"] as Array<Record<string, unknown>>;
		expect(flags).toEqual([
			{
				name: "verbose",
				description: "Verbose output",
				type: "boolean",
				extensionPath: ECHO_ENTRY,
				default: false,
			},
		]);

		const shortcuts = res["shortcuts"] as Array<Record<string, unknown>>;
		expect(shortcuts).toEqual([
			{
				key: "ctrl+alt+e",
				description: "Run the echo shortcut",
				extensionPath: ECHO_ENTRY,
			},
		]);

		const providers = res["providers"] as Array<Record<string, unknown>>;
		expect(providers).toHaveLength(1);
		expect(providers[0]).toMatchObject({
			name: "lean-provider",
			displayName: "Lean Provider",
			baseUrl: "https://example.invalid",
			streamSimple: true,
		});

		expect(res["handlers"]).toEqual(
			expect.arrayContaining(["session_start", "tool_call", "input"]),
		);
		expect(res["renderers"]).toEqual([]);
		expect(res["terminalInput"]).toBe(false);

		await link.finish();
	});

	test("CLI --extension load failures surface as extensionError events", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [FORBIDDEN_ENTRY] });
		await link.hello(1);
		const event = await link.waitFor(
			(f) => f.kind === "event" && f.method === "extensionError",
		);
		expect(String(payload(event)["message"])).toContain("excluded import");
		expect(link.runner.extensionCount).toBe(0);
		await link.finish();
	});
});

// ---------------------------------------------------------------------------
// tool.prepare / tool.validate / tool.execute / tool.cancel
// ---------------------------------------------------------------------------

describe("lean: tool RPCs", () => {
	async function loadedLink(): Promise<LeanLink> {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", { extensionPaths: [ECHO_ENTRY], cwd: PACKAGE_DIR });
		await link.response(2, "extensions.load");
		return link;
	}

	test("tool.prepare is a real RPC with the declared prepare step", async () => {
		const link = await loadedLink();
		link.request(10, "tool.prepare", { name: "echo", args: { text: "hi" } });
		const res = payload(await link.response(10, "tool.prepare"));
		expect(res["args"]).toEqual({ text: "hi", preparedBy: "lean" });
		await link.finish();
	});

	test("tool.prepare on an unknown tool is not_found", async () => {
		const link = await loadedLink();
		link.request(11, "tool.prepare", { name: "nope", args: {} });
		const err = payload(await link.error(11, "tool.prepare"));
		expect(err["code"]).toBe("not_found");
		await link.finish();
	});

	test("tool.validate returns normalized args or invalid_arguments", async () => {
		const link = await loadedLink();
		link.request(12, "tool.validate", { name: "echo", args: { text: "hi" } });
		const ok = payload(await link.response(12, "tool.validate"));
		expect(ok["args"]).toEqual({ text: "hi" });

		link.request(13, "tool.validate", { name: "echo", args: { text: 42 } });
		const err = payload(await link.error(13, "tool.validate"));
		expect(err["code"]).toBe("invalid_arguments");
		expect(String(err["message"])).toContain("echo.text must be a string");
		await link.finish();
	});

	test("tool.execute streams toolUpdate then resolves the tool result", async () => {
		const link = await loadedLink();
		link.request(14, "tool.execute", {
			name: "echo",
			toolCallId: "call-1",
			args: { text: "hi", preparedBy: "lean" },
			prepared: true,
		});
		const update = await link.waitFor(
			(f) => f.id === 14 && f.kind === "event" && f.method === "toolUpdate",
		);
		expect(payload(update)).toMatchObject({ toolCallId: "call-1", toolName: "echo" });
		const res = payload(await link.response(14, "tool.execute"));
		expect(res["content"]).toEqual([{ type: "text", text: "echo:hi" }]);
		expect(res["details"]).toMatchObject({ preparedBy: "lean", extensionPath: ECHO_ENTRY });
		await link.finish();
	});

	test("tool.execute honors tool.cancel with a cancelled error frame", async () => {
		const link = await loadedLink();
		link.request(15, "tool.execute", {
			name: "slow",
			toolCallId: "call-2",
			args: {},
			prepared: true,
		});
		// Synchronize on the tool's own started signal: the AbortController is
		// registered before execute runs, so the cancel cannot be lost.
		await link.waitFor(
			(f) => f.id === 15 && f.kind === "event" && f.method === "toolUpdate",
		);
		link.event("tool.cancel", { id: 15 });
		const err = payload(await link.error(15, "tool.execute"));
		expect(err["code"]).toBe("cancelled");
		await link.finish();
	});
});

// ---------------------------------------------------------------------------
// command.execute / flags.set / shortcut.execute / provider.stream
// ---------------------------------------------------------------------------

describe("lean: commands, flags, shortcuts, providers", () => {
	async function loadedLink(): Promise<LeanLink> {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", { extensionPaths: [ECHO_ENTRY], cwd: PACKAGE_DIR });
		await link.response(2, "extensions.load");
		return link;
	}

	test("command.execute runs the handler; unknown command is not_found", async () => {
		const link = await loadedLink();
		link.request(20, "command.execute", { command: "greet", args: "hello world" });
		const res = payload(await link.response(20, "command.execute"));
		expect(res["ok"]).toBe(true);
		expect(markerLog()).toContainEqual({
			name: "command",
			value: { args: "hello world", cwd: PACKAGE_DIR },
		});

		link.request(21, "command.execute", { command: "nope", args: "" });
		const err = payload(await link.error(21, "command.execute"));
		expect(err["code"]).toBe("not_found");
		await link.finish();
	});

	test("flags.set validates values and feeds the next snapshot", async () => {
		const link = await loadedLink();
		link.request(22, "flags.set", { values: { verbose: true } });
		const res = payload(await link.response(22, "flags.set"));
		expect(res["ok"]).toBe(true);

		link.request(23, "flags.set", { values: { verbose: 7 } });
		const err = payload(await link.error(23, "flags.set"));
		expect(err["code"]).toBe("invalid_arguments");

		link.request(24, "extensions.load", { extensionPaths: [], cwd: PACKAGE_DIR });
		const snapshot = payload(await link.response(24, "extensions.load"));
		const flags = snapshot["flags"] as Array<Record<string, unknown>>;
		expect(flags[0]?.["value"]).toBe(true);
		await link.finish();
	});

	test("shortcut.execute reports handled and runs the handler", async () => {
		const link = await loadedLink();
		link.request(25, "shortcut.execute", { key: "ctrl+alt+e" });
		const res = payload(await link.response(25, "shortcut.execute"));
		expect(res["handled"]).toBe(true);

		link.request(26, "shortcut.execute", { key: "ctrl+alt+zzz" });
		const miss = payload(await link.response(26, "shortcut.execute"));
		expect(miss["handled"]).toBe(false);

		// The shortcut handler runs fire-and-forget after the response is
		// enqueued; by the time the response frame is observed, the handler
		// microtask has already run.
		expect(markerLog()).toContainEqual({
			name: "shortcut",
			value: { cwd: PACKAGE_DIR },
		});
		await link.finish();
	});

	test("provider.stream forwards providerEvent frames then resolves", async () => {
		const link = await loadedLink();
		link.request(27, "provider.stream", {
			providerId: "lean-provider",
			model: { id: "m1" },
			context: {},
			options: {},
		});
		const events: Frame[] = [];
		events.push(await link.waitFor(
			(f) => f.id === 27 && f.kind === "event" && f.method === "providerEvent",
		));
		events.push(await link.waitFor(
			(f) => f.id === 27
				&& f.kind === "event"
				&& f.method === "providerEvent"
				&& (f.payload as Record<string, unknown>)["type"] === "done",
		));
		expect(payload(events[0] as Frame)["type"]).toBe("start");
		const res = payload(await link.response(27, "provider.stream"));
		expect(res).toEqual({});
		await link.finish();
	});
});

// ---------------------------------------------------------------------------
// Declared lifecycle hooks
// ---------------------------------------------------------------------------

describe("lean: lifecycle hooks", () => {
	async function loadedLink(): Promise<LeanLink> {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", { extensionPaths: [ECHO_ENTRY], cwd: PACKAGE_DIR });
		await link.response(2, "extensions.load");
		return link;
	}

	test("tool_call threads the mutable input and the handler result", async () => {
		const link = await loadedLink();
		link.request(30, "tool_call", {
			toolName: "echo",
			toolCallId: "call-9",
			input: { text: "hi" },
		});
		const res = payload(await link.response(30, "tool_call"));
		expect(res["block"]).toBe(false);
		expect(res["input"]).toEqual({ text: "hi", patched: true });
		await link.finish();
	});

	test("input hook returns the continue action", async () => {
		const link = await loadedLink();
		link.request(31, "input", { text: "hello", source: "interactive" });
		const res = payload(await link.response(31, "input"));
		expect(res["action"]).toBe("continue");
		await link.finish();
	});

	test("generic lifecycle events run declared hooks and ack", async () => {
		const link = await loadedLink();
		link.request(32, "session_start", { reason: "startup" });
		const res = payload(await link.response(32, "session_start"));
		expect(res["ok"]).toBe(true);
		expect(markerLog()).toContainEqual({ name: "hook.session_start", value: true });
		await link.finish();
	});

	test("undeclared methods are rejected as unknown", async () => {
		const link = await loadedLink();
		link.request(33, "turn_end", {});
		const err = payload(await link.error(33, "turn_end"));
		expect(err["code"]).toBe("extension_error");
		expect(String(err["message"])).toContain("unknown method: turn_end");
		await link.finish();
	});

	test("orphan message_update_delta errors and never invokes the hook", async () => {
		const link = await loadedLink();
		// A non-start delta before any assistant start must fail exactly like
		// Mode 1: extension_error, no synthesized assistant, no hook call.
		link.request(34, "message_update_delta", {
			type: "message_update_delta",
			event: { type: "text_delta", meta: {}, contentIndex: 0, delta: "hi" },
		});
		const err = payload(await link.error(34, "message_update_delta"));
		expect(err["code"]).toBe("extension_error");
		expect(String(err["message"])).toContain(
			"message update arrived before assistant start",
		);
		expect(markerLog().some((m) => m.name === "hook.message_update")).toBe(false);
		await link.finish();
	});

	test("start-then-delta reconstructs and invokes message_update", async () => {
		const link = await loadedLink();
		link.request(35, "message_update_delta", {
			type: "message_update_delta",
			event: { type: "start", meta: { role: "assistant" } },
		});
		const started = payload(await link.response(35, "message_update_delta"));
		expect(started["ok"]).toBe(true);

		link.request(36, "message_update_delta", {
			type: "message_update_delta",
			event: { type: "text_delta", meta: {}, contentIndex: 0, delta: "hi" },
		});
		const delta = payload(await link.response(36, "message_update_delta"));
		expect(delta["ok"]).toBe(true);
		expect(markerLog().filter((m) => m.name === "hook.message_update")).toHaveLength(2);
		await link.finish();
	});
});

// ---------------------------------------------------------------------------
// Ordered folds: later handlers must receive running values (two extensions)
// ---------------------------------------------------------------------------

describe("lean: ordered folds", () => {
	async function foldLink(): Promise<LeanLink> {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", {
			extensionPaths: [FOLD_FIRST_ENTRY, FOLD_SECOND_ENTRY],
			cwd: PACKAGE_DIR,
		});
		await link.response(2, "extensions.load");
		return link;
	}

	test("input: second handler receives the running text and images", async () => {
		const link = await foldLink();
		link.request(40, "input", {
			text: "hi",
			images: [{ marker: "base" }],
			source: "interactive",
		});
		const res = payload(await link.response(40, "input"));
		expect(res).toEqual({
			action: "transform",
			text: "hi|first|second",
			images: [{ marker: "base" }, { marker: "first" }],
		});
		expect(markerLog()).toContainEqual({
			name: "first.input",
			value: { text: "hi", images: [{ marker: "base" }] },
		});
		expect(markerLog()).toContainEqual({
			name: "second.input",
			value: { text: "hi|first", images: [{ marker: "base" }, { marker: "first" }] },
		});
		await link.finish();
	});

	test("before_agent_start: second handler receives the running systemPrompt", async () => {
		const link = await foldLink();
		link.event("session.update", { systemPrompt: "base" });
		link.request(41, "before_agent_start", { prompt: "p" });
		const res = payload(await link.response(41, "before_agent_start"));
		expect(res).toEqual({ systemPrompt: "base|first|second" });
		expect(markerLog()).toContainEqual({ name: "first.before_agent_start", value: "base" });
		expect(markerLog()).toContainEqual({
			name: "second.before_agent_start",
			value: "base|first",
		});
		await link.finish();
	});

	test("before_agent_start: incoming payload systemPrompt seeds the fold", async () => {
		const link = await foldLink();
		link.event("session.update", { systemPrompt: "base" });
		link.request(43, "before_agent_start", { prompt: "p", systemPrompt: "wire" });
		const res = payload(await link.response(43, "before_agent_start"));
		// The wire seed wins over the session mirror; the fold still threads it.
		expect(res).toEqual({ systemPrompt: "wire|first|second" });
		await link.finish();
	});

	test("tool_result: second handler receives running content/details/isError", async () => {
		const link = await foldLink();
		link.request(42, "tool_result", {
			toolName: "echo",
			toolCallId: "call-fold",
			input: {},
			content: ["base"],
			details: { origin: "base" },
			isError: false,
		});
		const res = payload(await link.response(42, "tool_result"));
		expect(res).toEqual({
			content: ["base", "first", "second"],
			details: { origin: "base", first: true },
			isError: false,
		});
		expect(markerLog()).toContainEqual({
			name: "first.tool_result",
			value: { content: ["base"], details: { origin: "base" }, isError: false },
		});
		expect(markerLog()).toContainEqual({
			name: "second.tool_result",
			value: {
				content: ["base", "first"],
				details: { origin: "base", first: true },
				isError: true,
			},
		});
		await link.finish();
	});

	test("message_end: second handler receives the running message", async () => {
		const link = await foldLink();
		link.request(43, "message_end", {
			message: { role: "assistant", content: "base" },
		});
		const res = payload(await link.response(43, "message_end"));
		expect(res["message"]).toEqual({ role: "assistant", content: "base|first|second" });
		expect(markerLog()).toContainEqual({
			name: "first.message_end",
			value: { role: "assistant", content: "base" },
		});
		expect(markerLog()).toContainEqual({
			name: "second.message_end",
			value: { role: "assistant", content: "base|first" },
		});
		await link.finish();
	});
});

// ---------------------------------------------------------------------------
// Surface validation + import-exclusion units
// ---------------------------------------------------------------------------

describe("lean: surface validation units", () => {
	test("parseLeanExtension rejects unknown top-level keys", () => {
		expect(() => parseLeanExtension({ bogus: true })).toThrow(
			'unknown key "bogus"',
		);
	});

	test("parseLeanExtension rejects unknown nested keys", () => {
		expect(() =>
			parseLeanExtension({
				providers: [{ name: "p", baseURL: "https://x" }],
			}),
		).toThrow('unknown key "baseURL"');
	});

	test("parseLeanExtension rejects unknown hook events", () => {
		expect(() =>
			parseLeanExtension({ hooks: { not_a_hook: () => {} } }),
		).toThrow('unknown lifecycle event "not_a_hook"');
	});

	test("parseLeanExtension accepts a full valid definition", () => {
		const definition = parseLeanExtension({
			name: "ok",
			tools: [{ name: "t", description: "d", execute: () => ({}) }],
			commands: [{ name: "c", handler: () => {} }],
			flags: [{ name: "f", type: "boolean", default: false }],
			shortcuts: [{ key: "ctrl+x", handler: () => {} }],
			providers: [{ name: "p", streamSimple: async function* () {} }],
			hooks: { session_start: () => {} },
		});
		expect(definition.name).toBe("ok");
	});

	test("findExcludedImport detects the compat graph and tolerates clean code", () => {
		expect(
			findExcludedImport('import { x } from "@earendil-works/pi-coding-agent/builtins";'),
		).toBe("@earendil-works/pi-coding-agent/builtins");
		expect(findExcludedImport('const m = await import("jiti");')).toBe("jiti");
		expect(findExcludedImport('import "./host.ts";')).toBe("./host.ts");
		expect(
			findExcludedImport('import { y } from "@earendil-works/pi-tui-protocol";'),
		).toBeUndefined();
		expect(findExcludedImport("export default { name: 'clean' };")).toBeUndefined();
	});

	test("parseStreamingJson tolerates truncated streams", () => {
		expect(parseStreamingJson('{"a":1,"b":[true,"x"]}')).toEqual({ a: 1, b: [true, "x"] });
		// Close-and-trim best effort: a truncated array closes empty rather
		// than dropping the key (documented tolerant behavior).
		expect(parseStreamingJson('{"a":1,"b":[tru')).toEqual({ a: 1, b: [] });
		expect(parseStreamingJson('{"a":"hel')).toEqual({ a: "hel" });
		expect(parseStreamingJson("garbage")).toEqual({});
		expect(parseStreamingJson(undefined)).toEqual({});
	});
});

// ---------------------------------------------------------------------------
// Structural proof: lean selection never evaluates the compat graph
// ---------------------------------------------------------------------------

const STATIC_IMPORT =
	/^\s*import\s+(?!type\b)(?:[^"';]*?\s+from\s+)?["']([^"']+)["']/gm;
const STATIC_REEXPORT = /^\s*export\s+[^"';]*?\s+from\s+["']([^"']+)["']/gm;

function staticSpecifiers(source: string): string[] {
	const specifiers: string[] = [];
	for (const pattern of [STATIC_IMPORT, STATIC_REEXPORT]) {
		pattern.lastIndex = 0;
		for (const match of source.matchAll(pattern)) {
			if (match[1] !== undefined) specifiers.push(match[1]);
		}
	}
	return specifiers;
}

/** Walk the static import graph from `entry`, returning visited files and bare specifiers. */
function walkStaticGraph(entry: string): { files: Set<string>; bare: Set<string> } {
	const files = new Set<string>();
	const bare = new Set<string>();
	const queue = [entry];
	while (queue.length > 0) {
		const current = queue.pop();
		if (current === undefined || files.has(current)) continue;
		files.add(current);
		for (const specifier of staticSpecifiers(readFileSync(current, "utf8"))) {
			if (specifier.startsWith("./") || specifier.startsWith("../")) {
				queue.push(resolve(dirname(current), specifier));
			} else {
				bare.add(specifier);
			}
		}
	}
	return { files, bare };
}

describe("lean: structural graph proofs", () => {
	test("main.ts statically imports nothing and gates on --lean before dynamic import", () => {
		const source = readFileSync(resolve(PACKAGE_DIR, "src", "main.ts"), "utf8");
		expect(staticSpecifiers(source)).toEqual([]);
		expect(source).toContain('import("./lean-runner.ts")');
		expect(source).toContain('import("./host.ts")');
		expect(source).toContain('import("@earendil-works/pi-coding-agent/builtins")');
		expect(source).toContain('"--lean"');
		// The flag is parsed before either dynamic import can run.
		expect(source.indexOf("parseArgs(process.argv)")).toBeGreaterThan(-1);
		expect(source.indexOf("parseArgs(process.argv)")).toBeLessThan(
			source.indexOf('import("./lean-runner.ts")'),
		);
	});

	test("the lean static graph contains no compat or upstream modules", () => {
		const { files, bare } = walkStaticGraph(
			resolve(PACKAGE_DIR, "src", "lean-runner.ts"),
		);
		const basenames = [...files].map((file) => basename(file));
		expect(basenames.sort()).toEqual(["lean-api.ts", "lean-runner.ts", "protocol.ts"]);
		for (const specifier of bare) {
			const allowed = specifier === "@earendil-works/pi-tui-protocol"
				|| specifier.startsWith("node:");
			expect(allowed).toBe(true);
		}
	});
});

// ---------------------------------------------------------------------------
// Load-time proof: drive main.ts --lean under a forbidding preload
// ---------------------------------------------------------------------------

describe("lean: subprocess graph-absence proof", () => {
	test("main.ts --lean serves the full flow without resolving the compat graph", async () => {
		const scratch = await mkdtemp(join(tmpdir(), "lean-host-"));
		const resolveLog = join(scratch, "resolve.log");
		try {
			const child = spawn(
				process.execPath,
				[
					"--preload",
					PRELOAD,
					"src/main.ts",
					"--lean",
					"--cwd",
					PACKAGE_DIR,
					"--extension",
					ECHO_ENTRY,
				],
				{
					cwd: PACKAGE_DIR,
					env: { ...process.env, LEAN_RESOLVE_LOG: resolveLog },
					stdio: ["pipe", "pipe", "pipe"],
				},
			);
			let stderr = "";
			child.stderr.on("data", (chunk: Buffer) => {
				stderr += chunk.toString();
			});

			// Line-buffered frame reader over stdout.
			let buffer = "";
			const queue: Frame[] = [];
			const waiters: Array<(frame: Frame) => void> = [];
			child.stdout.on("data", (chunk: Buffer) => {
				buffer += chunk.toString();
				let newline = buffer.indexOf("\n");
				while (newline !== -1) {
					const line = buffer.slice(0, newline);
					buffer = buffer.slice(newline + 1);
					newline = buffer.indexOf("\n");
					if (line.trim() === "") continue;
					const frame = JSON.parse(line) as Frame;
					const waiter = waiters.shift();
					if (waiter !== undefined) {
						waiter(frame);
					} else {
						queue.push(frame);
					}
				}
			});
			const nextFrame = (): Promise<Frame> => {
				const queued = queue.shift();
				if (queued !== undefined) return Promise.resolve(queued);
				const { promise, resolve: resolveFrame } = Promise.withResolvers<Frame>();
				waiters.push(resolveFrame);
				return promise;
			};
			const send = (frame: Frame): void => {
				child.stdin.write(encodeFrameString(frame));
			};
			const withTimeout = async <T>(promise: Promise<T>, label: string): Promise<T> => {
				// Deadline guard only: every step otherwise synchronizes on real
				// protocol frames. The timer exists so a wedged child fails the
				// test with context instead of hanging the suite.
				const { promise: deadline, reject } = Promise.withResolvers<never>();
				const timer = setTimeout(
					() => reject(new Error(`timeout: ${label}; stderr so far: ${stderr}`)),
					15_000,
				);
				try {
					return await Promise.race([promise, deadline]);
				} finally {
					clearTimeout(timer);
				}
			};

			// Protocol-only hello: foreign compatibilityVersion is ignored.
			send({
				id: 1,
				kind: "req",
				method: "hello",
				payload: { protocolVersion: PROTOCOL_VERSION, compatibilityVersion: "0.0.0-lean" },
			});
			const ack = payload(await withTimeout(nextFrame(), "helloAck"));
			expect(ack["protocolVersion"]).toBe(PROTOCOL_VERSION);

			// Registry from the CLI-selected lean entry (empty runtime load).
			send({ id: 2, kind: "req", method: "extensions.load", payload: { extensionPaths: [] } });
			const registry = payload(await withTimeout(nextFrame(), "extensions.load"));
			const toolNames = (registry["tools"] as Array<Record<string, unknown>>).map(
				(t) => t["name"],
			);
			expect(toolNames).toEqual(["echo", "slow"]);
			expect(registry["errors"]).toEqual([]);

			send({ id: 3, kind: "req", method: "tool.prepare", payload: { name: "echo", args: { text: "hi" } } });
			const prepared = payload(await withTimeout(nextFrame(), "tool.prepare"));
			expect(prepared["args"]).toEqual({ text: "hi", preparedBy: "lean" });

			send({
				id: 4,
				kind: "req",
				method: "tool.execute",
				payload: {
					name: "echo",
					toolCallId: "sub-1",
					args: { text: "hi", preparedBy: "lean" },
					prepared: true,
				},
			});
			const update = payload(await withTimeout(nextFrame(), "toolUpdate"));
			expect(update["toolCallId"]).toBe("sub-1");
			const executed = payload(await withTimeout(nextFrame(), "tool.execute"));
			expect(executed["content"]).toEqual([{ type: "text", text: "echo:hi" }]);

			send({ id: 5, kind: "req", method: "command.execute", payload: { command: "greet", args: "from-subprocess" } });
			const command = payload(await withTimeout(nextFrame(), "command.execute"));
			expect(command["ok"]).toBe(true);

			child.stdin.end();
			const { promise: exited, resolve: resolveExit } = Promise.withResolvers<number>();
			child.once("exit", (code) => resolveExit(code ?? -1));
			const exitCode = await withTimeout(exited, "process exit");
			expect(exitCode).toBe(0);

			// Positive evidence: the lean graph resolved; the compat graph never did.
			const log = readFileSync(resolveLog, "utf8");
			expect(log).toContain("./lean-runner.ts");
			expect(log).not.toMatch(/host\.ts/);
			expect(log).not.toMatch(/virtual-modules/);
			expect(log).not.toMatch(/pi-coding-agent/);
			expect(log).not.toMatch(/pi-agent-core|pi-ai|@mariozechner|jiti|typebox/);
		} finally {
			await rm(scratch, { recursive: true, force: true });
		}
	}, 30_000);
});
