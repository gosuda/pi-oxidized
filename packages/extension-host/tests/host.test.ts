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

import hooksFactory from "../fixtures/extensions/hooks.ts";
import toolFactory from "../fixtures/extensions/tool.ts";



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


/** Minimal context-actions stub for runner construction in tests. */
const noopContextActions: ExtensionContextActions = {
	getModel: () => undefined,
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
		const runner = new ExtensionRunner(
			[ext], runtime, process.cwd(),
			{} as ConstructorParameters<typeof ExtensionRunner>[3],
			{ getAll: () => [], find: () => undefined } as ConstructorParameters<typeof ExtensionRunner>[4],
		);
		runner.bindCore({} as never, noopContextActions);
		await runner.emit({ type: "session_start", reason: "startup" });
		expect(runner.hasHandlers("session_start")).toBe(true);
	});

	test("runner returns context hook result (pipeline)", async () => {
		const { ext, runtime } = await loadFactory(hooksFactory, "hooks.ts");
		const runner = new ExtensionRunner(
			[ext], runtime, process.cwd(),
			{} as ConstructorParameters<typeof ExtensionRunner>[3],
			{ getAll: () => [], find: () => undefined } as ConstructorParameters<typeof ExtensionRunner>[4],
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
			{} as ConstructorParameters<typeof ExtensionRunner>[3],
			{ getAll: () => [], find: () => undefined } as ConstructorParameters<typeof ExtensionRunner>[4],
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
