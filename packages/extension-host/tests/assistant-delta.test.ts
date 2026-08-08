/**
 * Assistant-delta reducer hardening tests:
 * - StreamingJsonParser is byte-identical to whole-buffer parseStreamingJson
 *   at every prefix while scanning each character once (linear, not quadratic).
 * - The host endpoint routes compact assistant streams through the SAME
 *   AssistantDeltaReducer as the lean endpoint, so hostile contentIndex
 *   values are rejected identically on both paths.
 * - SessionManager bridge probing (log/stringify/hasOwnProperty) never throws.
 * - Async slot factories install through the one entry-then-recreate contract.
 * - A dropped replacement token surfaces as an extensionError on the spot.
 * - Shortcut handlers observe this invocation's cancellation via ctx.signal.
 */
import { describe, expect, test } from "bun:test";
import { Readable } from "node:stream";
import {
	PROTOCOL_VERSION,
	encodeFrameString,
	type Frame,
} from "@earendil-works/pi-tui-protocol";
import type { ExtensionContext, ExtensionFactory } from "@earendil-works/pi-coding-agent";
import { ExtensionHost } from "../src/host.ts";
import { COMPATIBILITY_VERSION } from "../src/version.ts";
import {
	AssistantDeltaReducer,
	StreamingJsonParser,
	parseStreamingJson,
} from "../src/assistant-delta.ts";

/** Incremental push must equal the batch parse at every prefix. */
const CORPUS = [
	`{"a":1,"b":[true,"x"]}`,
	`{"a":1,"b":[tru`,
	`{"a":"hel`,
	`{"a":truex`,
	`garbage`,
	`{"a":1,"b":"${"x".repeat(600)}`,
	`{"a":1,"b":${"!".repeat(600)}`,
	`{"a":1}]`,
	`{"a":"x\\`,
	`{"payload":"${"x".repeat(2000)}","broken":${"!".repeat(500)}`,
	"z".repeat(1000),
	`{"a":12`,
	`{"a":1e`,
	`{"a":1e5`,
	`{"a":-0.5e+10`,
	`{"a":01`,
	`{"a":nul`,
	`{"a":null`,
	`{"a":nullx`,
	`{"a":tru`,
	`{"a":true`,
	`[1,2,3`,
	`{"a":{"b":12`,
	`{"a":[1,{"b":"s`,
	`"topstring`,
	`12`,
	`12 `,
	`12 34`,
	`{"a":1} `,
	`{"a":1} garbage`,
	`{"a":"\\u12`,
	`{"a":"\\u123`,
	`{"a":"\\u1234`,
	`{"a":"\\q"}`,
	`{"a":"`,
	`{"a"`,
	`{"a":`,
	`{"a": `,
	`{`,
	`[`,
	``,
	` `,
	`{}`,
	`[]`,
	`[[]`,
	`{"k":"v","k2":[1,2,{"3":null}],"k3":-1.5e-3}`,
	`{"a":1,"b":2,`.repeat(50),
	`{"s":"${"ab\\n".repeat(300)}","t":[${"1,".repeat(300)}`,
];

describe("StreamingJsonParser: byte-identical to batch at every prefix", () => {
	test("char-by-char pushes match parseStreamingJson on the full prefix", () => {
		for (const full of CORPUS) {
			const parser = new StreamingJsonParser();
			for (let index = 0; index < full.length; index += 1) {
				const incremental = parser.push(full.charAt(index));
				const batch = parseStreamingJson(full.slice(0, index + 1));
				expect(incremental).toEqual(batch);
			}
		}
	}, 30_000);

	test("window-crossing streams forget old candidates identically", () => {
		const streams = [
			`{"a":1,"b":"${"x".repeat(2000)}","c":[1,2,3`,
			`[${"1,".repeat(400)}2`,
			`{"s":"${"y".repeat(1500)}","n":${"7".repeat(900)}`,
		];
		for (const full of streams) {
			for (const step of [1, 3, 7, 64]) {
				const parser = new StreamingJsonParser();
				let incremental: Record<string, unknown> = {};
				for (let index = 0; index < full.length; index += step) {
					incremental = parser.push(full.slice(index, index + step));
					expect(incremental).toEqual(parseStreamingJson(full.slice(0, index + step)));
				}
			}
		}
	});
});

describe("StreamingJsonParser: per-push work is proportional to new bytes", () => {
	test("scanned character count grows linearly, not quadratically", () => {
		const scannedAfter = (fragments: number): number => {
			const parser = new StreamingJsonParser();
			parser.push(`{"s":"`);
			for (let index = 0; index < fragments; index += 1) parser.push("x");
			return parser.scannedChars;
		};
		const small = scannedAfter(1000);
		const doubled = scannedAfter(2000);
		const quadrupled = scannedAfter(4000);
		// Linear growth doubles when the input doubles; a whole-buffer rescan
		// per fragment would quadruple it.
		expect(doubled / small).toBeLessThan(2.2);
		expect(quadrupled / small).toBeLessThan(4.4);
	});

	test("parse entries never exceed one per push and skip doomed strict attempts", () => {
		const parser = new StreamingJsonParser();
		let pushes = 0;
		const push = (fragment: string): void => {
			parser.push(fragment);
			pushes += 1;
		};
		push(`{"s":"`);
		for (let index = 0; index < 100; index += 1) push("x");
		expect(parser.parseAttempts).toBeLessThanOrEqual(pushes);
		// Incomplete documents never reach the strict JSON.parse path: every
		// parse so far was a recovery parse of a growing in-string boundary.
		const completed = new StreamingJsonParser();
		completed.push(`{"a":1}`);
		const attemptsAfterCompletion = completed.parseAttempts;
		expect(attemptsAfterCompletion).toBe(1);
		completed.push(" ");
		completed.push("\n");
		// Whitespace-only growth after completion reuses the cached document.
		expect(completed.parseAttempts).toBe(attemptsAfterCompletion);
	});

	test("reducer toolcall stream matches batch reconstruction", () => {
		const argument = `{"query":"select * from t where x = \\"v\\"","limit":50,"ok":true}`;
		const reducer = new AssistantDeltaReducer();
		reducer.applyAssistantDelta({ type: "start", meta: { role: "assistant" } });
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			contentIndex: 0,
			block: { type: "toolCall", id: "t1", name: "db", arguments: {} },
		});
		for (const char of argument) {
			reducer.applyAssistantDelta({ type: "toolcall_delta", contentIndex: 0, delta: char });
		}
		const message = reducer.getActiveAssistant();
		const content = message?.["content"];
		const block = Array.isArray(content) ? content[0] : undefined;
		expect(
			typeof block === "object" && block !== null
				? (block as Record<string, unknown>)["arguments"]
				: undefined,
		).toEqual(parseStreamingJson(argument));
	});
});

// ---------------------------------------------------------------------------
// Host protocol harness (mirrors host.test.ts; factories stay inline so this
// suite owns no shared fixtures).
// ---------------------------------------------------------------------------

class FrameCollector {
	readonly frames: Frame[] = [];
	private buffer = "";
	private readonly waiters: Array<{
		predicate: (f: Frame) => boolean;
		resolve: (f: Frame) => void;
		reject: (error: Error) => void;
		timer: ReturnType<typeof setTimeout>;
	}> = [];

	write(chunk: Uint8Array): void {
		this.buffer += new TextDecoder().decode(chunk);
		const lines = this.buffer.split("\n");
		this.buffer = lines.pop() ?? "";
		for (const line of lines) {
			if (line.trim().length === 0) continue;
			const frame = JSON.parse(line) as Frame;
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

// ---------------------------------------------------------------------------
// H1: the host endpoint rejects hostile contentIndex exactly like the reducer
// ---------------------------------------------------------------------------

describe("host message_update_delta: hostile contentIndex parity with the reducer", () => {
	const deltaStream: Array<Record<string, unknown>> = [
		{ type: "start", meta: { role: "assistant" } },
		{ type: "text_start", contentIndex: 0, block: { type: "text", text: "" } },
		{ type: "text_delta", contentIndex: 0, delta: "a" },
		// Hostile: gap-creating start (content has length 1, index 2 skips 1).
		{ type: "text_start", contentIndex: 2, block: { type: "text", text: "gap" } },
		// Hostile: string index.
		{ type: "text_delta", contentIndex: "0", delta: "b" },
		// Hostile: negative index.
		{ type: "text_delta", contentIndex: -1, delta: "c" },
		// Hostile: non-integer index.
		{ type: "text_delta", contentIndex: 0.5, delta: "d" },
		// Hostile: out-of-range index.
		{ type: "text_delta", contentIndex: 5, delta: "e" },
		// Legitimate traffic resumes unaffected.
		{ type: "text_delta", contentIndex: 0, delta: "f" },
		{ type: "text_end", contentIndex: 0, block: { type: "text", text: "af" } },
	];

	test("host hook payloads equal the bare reducer expansion for every event", async () => {
		const seen: Array<{ message: unknown; assistantMessageEvent: unknown }> = [];
		const captureFactory: ExtensionFactory = (pi) => {
			// refs.d.ts has no typed overload for message_update; the generic
			// overload types the event as unknown, so narrow it here.
			pi.on("message_update", (event: unknown) => {
				const payload = event as { message: unknown; assistantMessageEvent: unknown };
				seen.push({
					message: payload.message,
					assistantMessageEvent: payload.assistantMessageEvent,
				});
			});
		};
		const connected = await connectHost([captureFactory]);
		const { collector, stdin } = connected;

		let requestId = 10;
		for (const event of deltaStream) {
			const id = requestId;
			requestId += 1;
			push(stdin, {
				id, kind: "req", method: "message_update_delta",
				payload: { type: "message_update_delta", event },
			});
			await collector.awaitFrame((f) => f.id === id && f.kind === "res");
		}

		// Bare-reducer reference: the exact semantics the lean endpoint exposes.
		const reference = new AssistantDeltaReducer();
		const expected: Array<{ message: unknown; assistantMessageEvent: unknown }> = [];
		for (const event of deltaStream) {
			reference.applyAssistantDelta(event);
			const active = reference.getActiveAssistant();
			if (active === undefined) throw new Error("reference lost assistant");
			const message = structuredClone(active);
			expected.push({
				message,
				assistantMessageEvent: reference.expandAssistantEvent(event, message),
			});
		}
		expect(seen).toEqual(expected);
		// The hostile events were dropped, not applied: only the legitimate
		// text block survived, with exactly the two accepted deltas.
		const last = seen[seen.length - 1];
		const content = (last?.message as Record<string, unknown>)["content"];
		expect(Array.isArray(content) ? content.length : 0).toBe(1);
		const block = Array.isArray(content) ? content[0] : undefined;
		expect(
			typeof block === "object" && block !== null
				? (block as Record<string, unknown>)["text"]
				: undefined,
		).toBe("af");
		await teardown(connected);
	});

	test("a delta before any assistant start errors like the lean path", async () => {
		const connected = await connectHost([() => {}]);
		const { collector, stdin } = connected;
		push(stdin, {
			id: 30, kind: "req", method: "message_update_delta",
			payload: {
				type: "message_update_delta",
				event: { type: "text_delta", meta: {}, contentIndex: 0, delta: "hi" },
			},
		});
		const response = await collector.awaitFrame((f) => f.id === 30 && f.kind === "error");
		expect(payloadOf(response)["code"]).toBe("extension_error");
		expect(String(payloadOf(response)["message"])).toContain(
			"message update arrived before assistant start",
		);
		await teardown(connected);
	});
});

// ---------------------------------------------------------------------------
// H3: SessionManager bridge tolerates probing; real methods still throw
// ---------------------------------------------------------------------------

describe("host SessionManager bridge", () => {
	test("logging, JSON.stringify, and hasOwnProperty do not throw; real methods do", async () => {
		const probeFactory: ExtensionFactory = (pi) => {
			pi.registerCommand("managerProbe", {
				description: "Probe the SessionManager bridge for throwable traps",
				async handler(_args, ctx) {
					// setup receives only the SessionManager proxy by design; the
					// fresh context is the ReplacedSessionContext handed to
					// withSession. Build the report inside setup, capture it in
					// the closure, and emit it from withSession via that fresh
					// context — the originating ctx is stale after newSession.
					let report: Record<string, unknown> = {};
					await ctx.newSession({
						parentSession: "parent-1",
						setup: async (manager) => {
							report = {};
							report["stringified"] = `${manager}`;
							report["json"] = JSON.stringify(manager);
							report["hasOwn"] = manager.hasOwnProperty("anything");
							report["valueOfIsObject"] = typeof manager.valueOf() === "object";
							try {
								manager.getEntries();
								report["realMethodThrew"] = false;
							} catch (error) {
								report["realMethodThrew"] = true;
								report["realMethodMessage"] =
									error instanceof Error ? error.message : String(error);
							}
							// Assert the value the runtime actually reads: thenable
							// resolution consults only the `get` trap, not `has`.
							// A Proxy can answer `"then" in manager` with true while
							// `get` returns undefined.
							report["then"] = Reflect.get(manager, "then") === undefined
								? undefined
								: "present";
						},
						withSession: async (freshCtx) => {
							freshCtx.ui.notify(JSON.stringify(report), "info");
						},
					});
				},
			});
		};
		const connected = await connectHost([probeFactory]);
		const { collector, stdin } = connected;

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.newSession")
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "res", method: "session.newSession",
					payload: { cancelled: false, replacementToken: "tok-1" },
				});
			});
		push(stdin, {
			id: 40, kind: "req", method: "command.execute",
			payload: { command: "managerProbe", args: "" },
		});
		const notify = await collector.awaitFrame((f) => f.method === "notify");
		const report = JSON.parse(String(payloadOf(notify)["message"])) as Record<string, unknown>;
		expect(report["stringified"]).toBe("[object Object]");
		expect(report["json"]).toBe("{}");
		expect(report["hasOwn"]).toBe(false);
		expect(report["valueOfIsObject"]).toBe(true);
		expect(report["then"]).toBeUndefined();
		expect(report["realMethodThrew"]).toBe(true);
		expect(String(report["realMethodMessage"])).toContain("not supported");
		await teardown(connected);
	});
});

// ---------------------------------------------------------------------------
// H4: async slot factories install through the one factory contract
// ---------------------------------------------------------------------------

describe("host component slots", () => {
	test("an async footer factory installs via ui.setFooter()", async () => {
		const footerFactory: ExtensionFactory = (pi) => {
			pi.registerCommand("footerProbe", {
				description: "Install an async footer factory",
				async handler(_args, ctx) {
					// The pinned upstream type declares a synchronous factory; the
					// host runtime contract accepts a thenable (H4 regression).
					const ui = ctx.ui as { setFooter(factory: unknown): void };
					ui.setFooter(async () => ({
						render: () => ["async-footer-line"],
					}));
				},
			});
		};
		const connected = await connectHost([footerFactory]);
		const { collector, stdin } = connected;
		push(stdin, {
			id: 50, kind: "req", method: "command.execute",
			payload: { command: "footerProbe", args: "" },
		});
		const slot = await collector.awaitFrame(
			(f) => f.method === "uiSlot" && payloadOf(f)["key"] === "footer.extension",
		);
		const runs = payloadOf(slot)["runs"];
		const texts = Array.isArray(runs)
			? runs.flatMap((line) =>
				Array.isArray(line)
					? line.map((run) =>
						typeof run === "object" && run !== null
							? String((run as Record<string, unknown>)["text"] ?? "")
							: "")
					: [])
			: [];
		expect(texts.join("")).toContain("async-footer-line");
		await teardown(connected);
	});
});

// ---------------------------------------------------------------------------
// H5: a dropped replacement token surfaces as an extensionError on the spot
// ---------------------------------------------------------------------------

describe("host replacement token scope", () => {
	test("fire-and-forget replacement emits an extensionError instead of silently timing out", async () => {
		const dropFactory: ExtensionFactory = (pi) => {
			pi.registerCommand("dropProbe", {
				description: "Fire-and-forget a replacement call",
				async handler(_args, ctx) {
					void ctx.newSession({ parentSession: "parent-1" });
				},
			});
		};
		const connected = await connectHost([dropFactory]);
		const { collector, stdin } = connected;

		push(stdin, {
			id: 60, kind: "req", method: "command.execute",
			payload: { command: "dropProbe", args: "" },
		});
		// The handler did not await, so the command responds first and closes
		// the token scope. Answer the in-flight replacement only afterwards:
		// the late capture finds the scope closed and must be diagnosed.
		await collector.awaitFrame((f) => f.id === 60 && f.kind === "res");
		const request = await collector.awaitFrame(
			(f) => f.kind === "req" && f.method === "session.newSession",
		);
		push(stdin, {
			id: request.id, kind: "res", method: "session.newSession",
			payload: { cancelled: false, replacementToken: "tok-dropped" },
		});
		const errorFrame = await collector.awaitFrame(
			(f) => f.kind === "event" && f.method === "extensionError",
		);
		expect(String(payloadOf(errorFrame)["message"])).toContain("replacement token dropped");
		expect(
			collector.frames.some(
				(f) => f.kind === "event" && f.method === "session.replacementReady",
			),
		).toBe(false);
		await teardown(connected);
	});
});

// ---------------------------------------------------------------------------
// H6: shortcut handlers observe invocation cancellation via ctx.signal
// ---------------------------------------------------------------------------

describe("host shortcut cancellation", () => {
	test("a cancelled shortcut sees an aborted signal in its handler context", async () => {
		const observations: Array<{ defined: boolean; abortedAfterDispose: boolean }> = [];
		const { promise: handlerRan, resolve: markHandlerRan } = Promise.withResolvers<void>();
		const shortcutFactory: ExtensionFactory = (pi) => {
			pi.registerShortcut("ctrl+alt+q", {
				description: "Signal probe",
				handler: (ctx: ExtensionContext) => {
					const signal = ctx.signal;
					if (signal === undefined) {
						observations.push({ defined: false, abortedAfterDispose: false });
						markHandlerRan();
						return;
					}
					// The response is sent before the handler body runs, so dispose
					// may have already aborted: read the signal instead of racing it.
					if (signal.aborted) {
						observations.push({ defined: true, abortedAfterDispose: true });
						markHandlerRan();
						return;
					}
					signal.addEventListener("abort", () => {
						observations.push({ defined: true, abortedAfterDispose: signal.aborted });
						markHandlerRan();
					}, { once: true });
				},
			});
		};
		const connected = await connectHost([shortcutFactory]);
		const { collector, stdin } = connected;
		push(stdin, {
			id: 70, kind: "req", method: "shortcut.execute",
			payload: { key: "ctrl+alt+q" },
		});
		const response = await collector.awaitFrame((f) => f.id === 70 && f.kind === "res");
		expect(payloadOf(response)["handled"]).toBe(true);
		// Disposal aborts in-flight shortcuts; the handler's abort listener is
		// the deterministic completion signal (no wall-clock waiting).
		connected.host.dispose("test");
		await handlerRan;
		expect(observations).toEqual([{ defined: true, abortedAfterDispose: true }]);
		connected.stdin.push(null);
		await connected.runPromise.catch(() => void 0);
	});
});
