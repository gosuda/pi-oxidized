import { afterEach, describe, expect, test } from "bun:test";
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

	request(id: number, method: string, payload: unknown): Promise<Frame> {
		this.stdin.push(Buffer.from(encodeFrameString({ id, kind: "req", method, payload })));
		const existing = this.frames.find((frame) => frame.id === id && frame.kind !== "req");
		if (existing !== undefined) return Promise.resolve(existing);
		const { promise, resolve: settle } = Promise.withResolvers<Frame>();
		this.waiters.push({ predicate: (frame) => frame.id === id && frame.kind !== "req", resolve: settle });
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

function mode1Factory(withHook: boolean): InlineExtension[] {
	if (!withHook) return [];
	const factory: ExtensionFactory = (pi) => {
		pi.on("message_update", (event) => {
			const holder = globalThis as Record<string, unknown>;
			const log = Array.isArray(holder[OBSERVATIONS]) ? holder[OBSERVATIONS] : [];
			log.push(structuredClone(event));
			holder[OBSERVATIONS] = log;
		});
		pi.on("before_agent_start", (event) => {
			const holder = globalThis as Record<string, unknown>;
			const log = Array.isArray(holder[OBSERVATIONS]) ? holder[OBSERVATIONS] : [];
			log.push({ type: event.type, systemPrompt: event.systemPrompt, cwd: event.systemPromptOptions.cwd });
			holder[OBSERVATIONS] = log;
			return {
				message: { role: "user", content: "injected" },
				systemPrompt: `${event.systemPrompt}|${event.systemPromptOptions.cwd}`,
			};
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
		for (const [index, event] of events.entries()) {
			const frame = await link.request(10 + index, "message_update_delta", {
				type: "message_update_delta",
				event,
			});
			responses.push({ kind: frame.kind, method: frame.method, payload: frame.payload });
		}
		const late = await link.request(20, "message_update_delta", {
			type: "message_update_delta",
			event: { type: "text_delta", meta: META, contentIndex: 0, delta: "late" },
		});
		responses.push({ kind: late.kind, method: late.method, payload: late.payload });
		return {
			responses,
			observations: structuredClone((globalThis as Record<string, unknown>)[OBSERVATIONS] ?? []),
		};
	} finally {
		await link.finish();
	}
}

afterEach(() => {
	delete (globalThis as Record<string, unknown>)[OBSERVATIONS];
});

describe("extension endpoint conformance", () => {
	test("identical assistant frames reconstruct identical hook payloads and responses", async () => {
		const mode1 = await runDeltaVector("mode1", true);
		const mode2 = await runDeltaVector("mode2", true);
		expect(mode2).toEqual(mode1);
	});

	test("the identical vector transitions and clears state without a message_update hook", async () => {
		const mode1 = await runDeltaVector("mode1", false);
		const mode2 = await runDeltaVector("mode2", false);
		expect(mode2).toEqual(mode1);
		expect(mode1.observations).toEqual([]);
		expect(mode1.responses.at(-1)?.kind).toBe("error");
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
