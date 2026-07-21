/**
 * Session/UI bridge acceptance: mirrored synchronous getters served from
 * `session.update` / `ui.state` pushes, fire-and-forget `session.command` /
 * `ui.control` emission, the correlated `session.setModel` and
 * `session.compact` round-trips, footer/header component slots, and typebox
 * virtual-module aliases.
 */

import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { Readable } from "node:stream";
import {
	PROTOCOL_VERSION,
	encodeFrameString,
	type Frame,
} from "@earendil-works/pi-tui-protocol";
import type { ExtensionFactory } from "@earendil-works/pi-coding-agent";
import { ExtensionHost } from "../src/host.ts";
import { COMPATIBILITY_VERSION } from "../src/version.ts";

import sessionActionsFactory, { abortProbeFactory } from "../fixtures/extensions/session-actions.ts";

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

async function connectHost(
	factories: ExtensionFactory[],
	extensionPaths: string[] = [],
): Promise<Connected> {
	const collector = new FrameCollector();
	const stdin = new Readable({ read() {} });
	const host = new ExtensionHost(stdin, collector);
	const runPromise = host.run({ cwd: process.cwd(), factories, extensionPaths });

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

const SESSION_UPDATE: Frame = {
	id: 0,
	kind: "event",
	method: "session.update",
	payload: {
		sessionName: "My Session",
		thinkingLevel: "high",
		activeTools: ["read", "bash"],
		allTools: [
			{ name: "read", description: "Read a file", parameters: { type: "object" }, source: "builtin" },
		],
		commands: [{ name: "review", description: "Review changes", source: "extension" }],
		model: { id: "gpt-x", provider: "openai" },
		isIdle: true,
		hasPendingMessages: false,
		contextUsage: { tokens: 1200, contextWindow: 128000, percent: 0.9 },
		systemPrompt: "You are pi.",
	},
};

function payloadOf(frame: Frame): Record<string, unknown> {
	return frame.payload as Record<string, unknown>;
}

function notifyReport(frame: Frame): Record<string, unknown> {
	return JSON.parse(String(payloadOf(frame)["message"])) as Record<string, unknown>;
}

describe("bridge: mirrored session getters + fire-and-forget commands", () => {
	test("sessionProbe observes pushed mirror and emits session commands", async () => {
		const connected = await connectHost([sessionActionsFactory]);
		const { collector, stdin } = connected;
		push(stdin, SESSION_UPDATE);

		// Answer the probe's pi.setModel round-trip when it arrives.
		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.setModel")
			.then((request) => {
				expect(payloadOf(request)["model"]).toEqual({ id: "probe-model", provider: "probe" });
				push(stdin, {
					id: request.id, kind: "res", method: "session.setModel",
					payload: { success: true },
				});
			});

		push(stdin, {
			id: 20, kind: "req", method: "command.execute",
			payload: { command: "sessionProbe", args: "" },
		});
		const notify = await collector.awaitFrame((f) => f.method === "notify");
		await collector.awaitFrame((f) => f.id === 20 && f.kind === "res");

		const report = notifyReport(notify);
		expect(report["sessionName"]).toBe("My Session");
		expect(report["thinkingLevel"]).toBe("high");
		expect(report["activeTools"]).toEqual(["read", "bash"]);
		expect(report["allTools"]).toEqual(["read"]);
		expect(report["commands"]).toEqual(["review"]);
		expect(report["isIdle"]).toBe(true);
		expect(report["hasPending"]).toBe(false);
		expect(report["systemPrompt"]).toBe("You are pi.");
		expect(report["model"]).toBe("gpt-x");
		expect(report["setModel"]).toBe(true);
		expect(report["signal"]).toBe("none");
		expect(report["contextUsage"]).toEqual({ tokens: 1200, contextWindow: 128000, percent: 0.9 });
		// Setter-then-getter coherence within the same handler (optimistic
		// local mirror applied before the fire-and-forget command).
		expect(report["nameAfterSet"]).toBe("probe-renamed");
		expect(report["activeAfterSet"]).toEqual(["read"]);
		expect(report["levelAfterSet"]).toBe("low");

		const actions = collector.frames
			.filter((f) => f.method === "session.command")
			.map((f) => payloadOf(f)["action"]);
		expect(actions).toEqual([
			"setSessionName",
			"setLabel",
			"appendEntry",
			"setActiveTools",
			"setThinkingLevel",
			"sendMessage",
			"sendUserMessage",
		]);
		const byAction = new Map(
			collector.frames
				.filter((f) => f.method === "session.command")
				.map((f) => [payloadOf(f)["action"], payloadOf(f)] as const),
		);
		expect(byAction.get("setSessionName")?.["name"]).toBe("probe-renamed");
		expect(byAction.get("setLabel")).toMatchObject({ entryId: "entry-1", label: "flagged" });
		expect(byAction.get("appendEntry")).toMatchObject({ customType: "probe", data: { marker: 7 } });
		expect(byAction.get("setActiveTools")?.["toolNames"]).toEqual(["read"]);
		expect(byAction.get("setThinkingLevel")?.["level"]).toBe("low");
		expect(byAction.get("sendMessage")).toMatchObject({
			message: { customType: "probe", content: "hello", display: true },
			options: { deliverAs: "nextTurn" },
		});
		expect(byAction.get("sendUserMessage")).toMatchObject({
			content: "user text",
			options: { deliverAs: "followUp" },
		});

		await teardown(connected);
	});

	test("idle→busy transition arms the turn signal; abort trips it and forwards", async () => {
		const connected = await connectHost([abortProbeFactory]);
		const { collector, stdin } = connected;

		push(stdin, SESSION_UPDATE);
		push(stdin, {
			...SESSION_UPDATE,
			payload: { ...(SESSION_UPDATE.payload as Record<string, unknown>), isIdle: false },
		});

		push(stdin, {
			id: 21, kind: "req", method: "command.execute",
			payload: { command: "abortProbe", args: "" },
		});
		const notify = await collector.awaitFrame((f) => f.method === "notify");
		const report = notifyReport(notify);
		expect(report["before"]).toBe("false");
		expect(report["after"]).toBe("true");

		const abortCommand = collector.frames.find(
			(f) => f.method === "session.command" && payloadOf(f)["action"] === "abort",
		);
		expect(abortCommand).toBeDefined();

		await teardown(connected);
	});
});

describe("bridge: ui.control surface + ui.state mirror", () => {
	test("uiProbe emits controls, mirrors editor text, and pushes footer/header slots", async () => {
		const connected = await connectHost([sessionActionsFactory]);
		const { collector, stdin } = connected;

		push(stdin, {
			id: 0, kind: "event", method: "ui.state",
			payload: { editorText: "", toolsExpanded: false },
		});
		push(stdin, {
			id: 22, kind: "req", method: "command.execute",
			payload: { command: "uiProbe", args: "" },
		});
		const notify = await collector.awaitFrame((f) => f.method === "notify");
		const report = notifyReport(notify);
		expect(report["editorAfterSet"]).toBe("draft");
		expect(report["editorAfterPaste"]).toBe("draft+more");
		expect(report["toolsExpanded"]).toBe(true);

		const controls = collector.frames
			.filter((f) => f.method === "ui.control")
			.map((f) => payloadOf(f)["control"]);
		expect(controls).toEqual([
			"setStatus",
			"setWorkingMessage",
			"setWorkingVisible",
			"setHiddenThinkingLabel",
			"setTitle",
			"setEditorText",
			"pasteToEditor",
			"setToolsExpanded",
		]);
		const status = collector.frames.find(
			(f) => f.method === "ui.control" && payloadOf(f)["control"] === "setStatus",
		);
		expect(status).toBeDefined();
		if (status !== undefined) {
			expect(payloadOf(status)).toMatchObject({ key: "lint", text: "3 warnings" });
		}

		const footer = await collector.awaitFrame(
			(f) => f.method === "uiSlot" && payloadOf(f)["placement"] === "footer",
		);
		expect(payloadOf(footer)["key"]).toBe("footer.extension");
		const header = await collector.awaitFrame(
			(f) => f.method === "uiSlot" && payloadOf(f)["placement"] === "header",
		);
		expect(payloadOf(header)["key"]).toBe("header.extension");

		await teardown(connected);
	});
});

describe("bridge: correlated compact", () => {
	test("compact resolves onComplete from the response and onError from an error frame", async () => {
		const connected = await connectHost([sessionActionsFactory]);
		const { collector, stdin } = connected;

		void collector
			.awaitFrame((f) => f.kind === "req" && f.method === "session.compact")
			.then((request) => {
				expect(payloadOf(request)["customInstructions"]).toBe("keep decisions");
				push(stdin, {
					id: request.id, kind: "res", method: "session.compact",
					payload: { result: { summary: "s", firstKeptEntryId: "e9" } },
				});
			});
		push(stdin, {
			id: 23, kind: "req", method: "command.execute",
			payload: { command: "compactProbe", args: "" },
		});
		const okNotify = await collector.awaitFrame((f) => f.method === "notify");
		expect(notifyReport(okNotify)).toMatchObject({
			compact: "ok",
			result: { summary: "s", firstKeptEntryId: "e9" },
		});

		// Second run: Rust reports failure via an error frame → onError.
		const seenCompacts = collector.frames.filter(
			(f) => f.kind === "req" && f.method === "session.compact",
		).length;
		void collector
			.awaitFrame(
				(f) =>
					f.kind === "req"
					&& f.method === "session.compact"
					&& collector.frames.filter(
						(g) => g.kind === "req" && g.method === "session.compact",
					).length > seenCompacts,
			)
			.then((request) => {
				push(stdin, {
					id: request.id, kind: "error", method: "session.compact",
					payload: { code: "extension_error", message: "no active session", retryable: false },
				});
			});
		push(stdin, {
			id: 24, kind: "req", method: "command.execute",
			payload: { command: "compactProbe", args: "" },
		});
		const errNotify = await collector.awaitFrame(
			(f) => f.method === "notify" && String(payloadOf(f)["message"]).includes("\"err\""),
		);
		const errReport = notifyReport(errNotify);
		expect(errReport["compact"]).toBe("err");
		expect(String(errReport["message"])).toContain("no active session");

		await teardown(connected);
	});
});

describe("bridge: typebox virtual-module aliases", () => {
	test("extension importing typebox and @sinclair/typebox loads via jiti", async () => {
		const fixturePath = resolve(import.meta.dir, "../fixtures/extensions/typebox-import.ts");
		const connected = await connectHost([]);
		const { collector, stdin, host } = connected;

		push(stdin, {
			id: 30, kind: "req", method: "extensions.load",
			payload: { extensionPaths: [fixturePath], cwd: process.cwd(), projectTrusted: true },
		});
		const response = await collector.awaitFrame((f) => f.id === 30 && f.kind === "res");
		const payload = payloadOf(response);
		expect(payload["errors"]).toEqual([]);
		expect(payload["extensions"]).toBe(1);
		const tools = payload["tools"] as Array<{ name: string; description: string }>;
		const probe = tools.find((tool) => tool.name === "typeboxProbe");
		expect(probe).toBeDefined();
		expect(probe?.description).toBe("valid=true");

		const tool = host.getRunner()?.getToolDefinition("typeboxProbe");
		expect(tool).toBeDefined();
		expect((tool?.parameters as Record<string, unknown> | undefined)?.["type"]).toBe("object");

		await teardown(connected);
	});
});

describe("bridge: pi-ai root image exports", () => {
	test("extension imports image generation and registries through the root alias", async () => {
		const fixturePath = resolve(import.meta.dir, "../fixtures/extensions/pi-ai-images-import.ts");
		const connected = await connectHost([]);
		const { collector, stdin } = connected;

		push(stdin, {
			id: 31, kind: "req", method: "extensions.load",
			payload: { extensionPaths: [fixturePath], cwd: process.cwd(), projectTrusted: true },
		});
		const response = await collector.awaitFrame((frame) => frame.id === 31 && frame.kind === "res");
		const payload = payloadOf(response);
		expect(payload["errors"]).toEqual([]);
		expect(payload["extensions"]).toBe(1);
		const tools = payload["tools"] as Array<{ name: string; description: string }>;
		const probe = tools.find((tool) => tool.name === "piAiImagesProbe");
		expect(probe?.description).toBe("generate=true;models=true;api=true");

		await teardown(connected);
	});
});
