import { afterEach, describe, expect, test, vi } from "bun:test";
import { join, resolve } from "node:path";
import { Readable } from "node:stream";
import type { ExtensionFactory, InlineExtension } from "@earendil-works/pi-coding-agent";
import {
	COMPATIBILITY_VERSION,
	FrameDecoder,
	PROTOCOL_VERSION,
	encodeFrameString,
	type Frame,
} from "@earendil-works/pi-tui-protocol";
import { ExtensionHost } from "../src/host.ts";
import { LeanRunner } from "../src/lean-runner.ts";

const CWD = resolve(import.meta.dirname, "..");
const LEAN_CONFORMANCE = join(import.meta.dirname, "fixtures", "lean", "endpoint-conformance.mjs");
const OBSERVATIONS = "__endpointConformanceLog";

type Endpoint = {
	dispose(reason?: string): void;
};

class EndpointLink {
	private readonly stdin = new Readable({ read() {} });
	private readonly decoder = new FrameDecoder();
	private readonly frames: Frame[] = [];
	private readonly waiters: Array<{ predicate: (frame: Frame) => boolean; resolve: (frame: Frame) => void }> = [];
	private endpoint: Endpoint | undefined;
	private runPromise: Promise<void> = Promise.resolve();

	start(endpoint: Endpoint, runPromise: Promise<void>): void {
		this.endpoint = endpoint;
		this.runPromise = runPromise;
	}

	get input(): Readable {
		return this.stdin;
	}

	get output(): { write(chunk: Uint8Array): void } {
		return {
			write: (chunk) => {
				for (const frame of this.decoder.push(chunk)) {
					this.frames.push(frame);
					for (let index = this.waiters.length - 1; index >= 0; index--) {
						const waiter = this.waiters[index];
						if (waiter?.predicate(frame)) {
							this.waiters.splice(index, 1);
							waiter.resolve(frame);
						}
					}
				}
			},
		};
	}

	request(id: number, method: string, payload: unknown, timeoutMs = 30_000): Promise<Frame> {
		// `frames` accumulates for the lifetime of the link, so only frames that
		// arrive as a consequence of this push may settle this call. A lifetime
		// scan would latch onto a leftover reply carrying the same id.
		const before = this.frames.length;
		this.stdin.push(Buffer.from(encodeFrameString({ id, kind: "req", method, payload })));
		for (let index = before; index < this.frames.length; index++) {
			const arrived = this.frames[index];
			if (arrived !== undefined && arrived.id === id && arrived.kind !== "req") {
				return Promise.resolve(arrived);
			}
		}
		const { promise, resolve: settle, reject } = Promise.withResolvers<Frame>();
		let waiter: (typeof this.waiters)[number] | undefined;
		const timer = setTimeout(() => {
			if (waiter === undefined) return;
			const index = this.waiters.indexOf(waiter);
			if (index === -1) return;
			this.waiters.splice(index, 1);
			reject(new Error(`no response to ${method} (id ${id}) within ${timeoutMs}ms`));
		}, timeoutMs);
		waiter = {
			predicate: (frame) => frame.id === id && frame.kind !== "req",
			resolve: (frame) => {
				clearTimeout(timer);
				settle(frame);
			},
		};
		this.waiters.push(waiter);
		return promise;
	}

	event(method: string, payload: unknown): void {
		this.stdin.push(Buffer.from(encodeFrameString({ id: 0, kind: "event", method, payload })));
	}

	async finish(): Promise<void> {
		this.stdin.push(null);
		this.endpoint?.dispose("test");
		await this.runPromise.catch(() => undefined);
	}
}

/** Event shapes for lifecycle hooks not covered by the typed `ExtensionAPI.on` overloads. */
interface BeforeAgentStartHookEvent {
	readonly type: "before_agent_start";
	readonly prompt: string;
	readonly systemPrompt: string;
	readonly systemPromptOptions: { readonly cwd: string };
}

interface ToolCallHookEvent {
	readonly type: "tool_call";
	readonly toolName: string;
	readonly toolCallId: string;
	readonly input: Record<string, unknown>;
}

interface ToolResultHookEvent {
	readonly type: "tool_result";
	readonly toolName: string;
	readonly toolCallId: string;
	readonly input: Record<string, unknown>;
	readonly content: unknown[];
	readonly details: unknown;
	readonly isError: boolean;
}

function mode1Factory(withHook: boolean): InlineExtension[] {
	if (!withHook) return [];
	const factory: ExtensionFactory = (pi) => {
		pi.on("message_update", (event) => {
			observe(event);
		});
		pi.on("before_agent_start", (event) => {
			const e = event as BeforeAgentStartHookEvent;
			observe({ type: e.type, systemPrompt: e.systemPrompt, cwd: e.systemPromptOptions.cwd });
			const message = { role: "user" as const, content: "injected" };
			if (e.prompt === "no-system-prompt") return { message };
			if (e.prompt === "non-string-system-prompt") {
				return { message, systemPrompt: null };
			}
			return {
				message,
				systemPrompt: `${e.systemPrompt}|${e.systemPromptOptions.cwd}`,
			};
		});
		pi.on("tool_call", (event) => {
			observe({ type: event.type, toolName: event.toolName, toolCallId: event.toolCallId, input: event.input });
			event.input["fromHook"] = "tool-call";
		});
		pi.on("tool_result", (event) => {
			observe({
				type: event.type,
				toolName: event.toolName,
				toolCallId: event.toolCallId,
				input: event.input,
				content: event.content,
				details: event.details,
				isError: event.isError,
			});
			return {
				content: [{ type: "text", text: "rewritten tool result" }],
				details: { fromHook: true },
				isError: true,
			};
		});
		pi.on("message_end", (event) => {
			observe({ type: event.type, message: event.message });
			return { message: { role: "assistant", content: [{ type: "text", text: "rewritten message" }] } };
		});
		pi.on("input", (event) => {
			const input = event as typeof event & { type: string; text: string; images?: unknown };
			observe({ type: input.type, text: input.text, images: input.images, source: input.source });
			return { action: "transform", text: `${input.text} rewritten` };
		});
		pi.on("resources_discover" as string, (...args: unknown[]) => {
			const [event] = args as [{ type: string; cwd: string; reason: string }];
			observe({ type: event.type, cwd: event.cwd, reason: event.reason });
			return { skillPaths: ["/skills"], promptPaths: ["/prompts"], themePaths: ["/themes"] };
		});
		pi.on("session_before_tree" as string, (...args: unknown[]) => {
			const [event] = args as [{ type: string }];
			observe({ type: event.type });
			return { cancel: true, reason: "endpoint conformance" };
		});
		pi.registerShortcut("ctrl+shift+e", {
			handler() {
				observe({ type: "shortcut", key: "ctrl+shift+e" });
				return new Promise<void>((resolve) => setImmediate(resolve));
			},
		});
	};
	return [{ name: "endpoint-conformance", factory }];
}

async function openEndpoint(mode: "mode1" | "mode2", withHook: boolean): Promise<EndpointLink> {
	const link = new EndpointLink();
	if (mode === "mode1") {
		const endpoint = new ExtensionHost(link.input, link.output);
		link.start(endpoint, endpoint.run({ cwd: CWD, extensionPaths: [], factories: mode1Factory(withHook) }));
	} else {
		const endpoint = new LeanRunner(link.input, link.output);
		link.start(endpoint, endpoint.run({ cwd: CWD, extensionPaths: withHook ? [LEAN_CONFORMANCE] : [] }));
	}
	const hello = await link.request(1, "hello", {
		protocolVersion: PROTOCOL_VERSION,
		compatibilityVersion: COMPATIBILITY_VERSION,
	});
	expect(hello.kind).toBe("res");
	return link;
}

const META = {
	role: "assistant",
	api: "test-api",
	provider: "test-provider",
	model: "test-model",
	usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: {} },
	stopReason: "stop",
	timestamp: 1,
};

function observe(value: unknown): void {
	const holder = globalThis as Record<string, unknown>;
	const log = Array.isArray(holder[OBSERVATIONS]) ? holder[OBSERVATIONS] : [];
	log.push(structuredClone(value));
	holder[OBSERVATIONS] = log;
}

function normalizeExtensionOrigins<T>(value: T): T {
	return JSON.parse(JSON.stringify(
		value,
		(key, nested) => key === "extensionPath" ? "<extension>" : nested,
	)) as T;
}

async function runDeltaVector(mode: "mode1" | "mode2", withHook: boolean) {
	(globalThis as Record<string, unknown>)[OBSERVATIONS] = [];
	const link = await openEndpoint(mode, withHook);
	try {
		const final = { ...META, content: [{ type: "text", text: "hello" }] };
		const events: Array<Record<string, unknown>> = [
			{ type: "start", meta: META },
			{ type: "text_start", meta: META, contentIndex: 0, block: { type: "text", text: "" } },
			{ type: "text_delta", meta: META, contentIndex: 0, delta: "hello" },
			{ type: "text_end", meta: META, contentIndex: 0, block: { type: "text", text: "hello" } },
			{ type: "done", reason: "stop", final },
		];
		const responses: Array<{ kind: string; method: string; payload: unknown }> = [];
		const request = async (id: number, method: string, payload: unknown) => {
			const frame = await link.request(id, method, payload);
			responses.push({ kind: frame.kind, method: frame.method, payload: frame.payload });
		};
		for (const [index, event] of events.entries()) {
			await request(10 + index, "message_update_delta", { type: "message_update_delta", event });
		}
		await request(20, "message_update_delta", {
			type: "message_update_delta",
			event: { type: "text_delta", meta: META, contentIndex: 0, delta: "late" },
		});
		await request(21, "before_agent_start", { prompt: "vector", systemPrompt: "vector prompt" });
		await request(22, "tool_call", { toolName: "demo", toolCallId: "call-1", input: { value: 1 } });
		await request(23, "tool_result", {
			toolName: "demo",
			toolCallId: "call-1",
			input: { value: 1 },
			content: [{ type: "text", text: "original tool result" }],
			details: { original: true },
			isError: false,
		});
		await request(24, "message_end", { role: "assistant", content: [{ type: "text", text: "original message" }] });
		await request(25, "input", { text: "original input", source: "user" });
		await request(26, "resources_discover", { cwd: CWD, reason: "startup" });
		await request(27, "session_before_tree", {});
		const shortcuts = await Promise.all([
			link.request(28, "shortcut.execute", { key: "ctrl+shift+e" }),
			link.request(29, "shortcut.execute", { key: "ctrl+shift+e" }),
		]);
		for (const frame of shortcuts) {
			responses.push({ kind: frame.kind, method: frame.method, payload: frame.payload });
		}
		await new Promise<void>((resolve) => setImmediate(resolve));
		return {
			responses: normalizeExtensionOrigins(responses),
			observations: normalizeExtensionOrigins(
				structuredClone((globalThis as Record<string, unknown>)[OBSERVATIONS] ?? []) as unknown[],
			),
		};
	} finally {
		await link.finish();
	}
}

afterEach(() => {
	delete (globalThis as Record<string, unknown>)[OBSERVATIONS];
});

describe("extension endpoint conformance", () => {
	test("request deadlines name and remove the missing response waiter", async () => {
		const link = new EndpointLink();
		vi.useFakeTimers();
		try {
			let timeoutError: unknown;
			const pending = link.request(99, "missing.method", {}, 5);
			void pending.catch((error: unknown) => {
				timeoutError = error;
			});
			vi.advanceTimersByTime(5);
			await Promise.resolve();
			expect(timeoutError).toEqual(
				new Error("no response to missing.method (id 99) within 5ms"),
			);

			const clearTimeoutSpy = vi.spyOn(globalThis, "clearTimeout");
			link.output.write(
				Buffer.from(encodeFrameString({ id: 99, kind: "res", method: "missing.method", payload: {} })),
			);
			expect(clearTimeoutSpy).not.toHaveBeenCalled();
			clearTimeoutSpy.mockRestore();

			// A later call reusing the id must register a fresh waiter and settle
			// on a new reply; the unclaimed frame above must not satisfy it.
			const retried = link.request(99, "missing.method", {}, 5);
			link.output.write(
				Buffer.from(
					encodeFrameString({ id: 99, kind: "res", method: "missing.method", payload: { round: 2 } }),
				),
			);
			expect(await retried).toMatchObject({ id: 99, kind: "res", payload: { round: 2 } });
		} finally {
			vi.useRealTimers();
			await link.finish();
		}
	});

	test("identical assistant frames reconstruct identical hook payloads and responses", async () => {
		const mode1 = await runDeltaVector("mode1", true);
		const mode2 = await runDeltaVector("mode2", true);
		expect(mode1.responses).toContainEqual({
			kind: "res",
			method: "session_before_tree",
			payload: { cancel: true, reason: "endpoint conformance" },
		});
		const shortcuts = mode1.observations.filter((observation) =>
			JSON.stringify(observation) === JSON.stringify({ type: "shortcut", key: "ctrl+shift+e" }),
		);
		// Keyed single-flight: concurrent duplicate shortcut.execute shares one handler.
		expect(shortcuts).toHaveLength(1);
		expect(mode2).toEqual(mode1);
	});

	test("system prompt omission and non-string emission match", async () => {
		const run = async (mode: "mode1" | "mode2") => {
			const link = await openEndpoint(mode, true);
			try {
				return [
					await link.request(40, "before_agent_start", { prompt: "no-system-prompt" }),
					await link.request(41, "before_agent_start", { prompt: "non-string-system-prompt" }),
				];
			} finally {
				await link.finish();
			}
		};
		const mode1 = await run("mode1");
		const mode2 = await run("mode2");
		expect(mode2.map(encodeFrameString)).toEqual(mode1.map(encodeFrameString));
		expect(mode1.map((frame) => frame.payload)).toEqual([
			{ messages: [{ role: "user", content: "injected" }] },
			{ messages: [{ role: "user", content: "injected" }], systemPrompt: null },
		]);
	});

	test("system prompt wire precedence and mirror reset match", async () => {
		const run = async (mode: "mode1" | "mode2") => {
			(globalThis as Record<string, unknown>)[OBSERVATIONS] = [];
			const link = await openEndpoint(mode, true);
			try {
				link.event("session.update", { systemPrompt: "mirror" });
				const mirrored = await link.request(30, "before_agent_start", { prompt: "go" });
				link.event("session.update", {});
				const reset = await link.request(31, "before_agent_start", { prompt: "go" });
				const wired = await link.request(32, "before_agent_start", { prompt: "go", systemPrompt: "wire" });
				return [mirrored.payload, reset.payload, wired.payload];
			} finally {
				await link.finish();
			}
		};
		const expected = [
			{
				messages: [{ role: "user", content: "injected" }],
				systemPrompt: `mirror|${CWD}`,
			},
			{
				messages: [{ role: "user", content: "injected" }],
				systemPrompt: `|${CWD}`,
			},
			{
				messages: [{ role: "user", content: "injected" }],
				systemPrompt: `wire|${CWD}`,
			},
		];
		expect(await run("mode1")).toEqual(expected);
		expect(await run("mode2")).toEqual(expected);
	});
});
