/**
 * Lean Mode-2 tests: protocol-only hello, extensions.load registry,
 * prepare/validate/execute/cancel, commands, flags, shortcuts, providers,
 * lifecycle hooks, per-extension load-error isolation — plus the structural
 * and load-time proofs that lean selection never evaluates host.ts,
 * builtins, virtual-modules, or the upstream package graph.
 */
import { afterEach, describe, expect, test, vi } from "bun:test";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
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
import type { LeanExtension } from "../src/lean-api.ts";
import {
	findExcludedImport,
	LeanRunner,
} from "../src/lean-runner.ts";
import { AssistantDeltaReducer, parseStreamingJson } from "../src/assistant-delta.ts";

const PACKAGE_DIR = resolve(import.meta.dirname, "..");
const LEAN_FIXTURES = resolve(import.meta.dirname, "fixtures", "lean");
const ECHO_ENTRY = join(LEAN_FIXTURES, "echo.mjs");
const FORBIDDEN_ENTRY = join(LEAN_FIXTURES, "forbidden-import.mjs");
const FACTORY_ENTRY = join(LEAN_FIXTURES, "factory-surface.mjs");
const FOLD_FIRST_ENTRY = join(LEAN_FIXTURES, "fold-first.mjs");
const FOLD_SECOND_ENTRY = join(LEAN_FIXTURES, "fold-second.mjs");
const ROLE_BREAKER_ENTRY = join(LEAN_FIXTURES, "role-breaker.mjs");
const MESSAGE_UPDATE_CANCEL_ENTRY = join(LEAN_FIXTURES, "message-update-cancel.mjs");
const TOOL_CALL_NOOP_ENTRY = join(LEAN_FIXTURES, "tool-call-noop.mjs");
const TOOL_CALL_REORDER_ENTRY = join(LEAN_FIXTURES, "tool-call-reorder.mjs");
const TOOL_CALL_VALUE_CHANGE_ENTRY = join(LEAN_FIXTURES, "tool-call-value-change.mjs");
const FLOW_CONTROL_ENTRY = join(LEAN_FIXTURES, "flow-control.mjs");
const FLAG_CONTEXT_ENTRY = join(LEAN_FIXTURES, "flag-context.mjs");
const PRELOAD = resolve(import.meta.dirname, "fixtures", "lean-forbid-compat-graph.ts");

type Marker = { name: string; value: unknown };

function markerLog(): Marker[] {
	const key = "__leanEchoLog";
	const holder = globalThis as Record<string, unknown>;
	const log = holder[key];
	return Array.isArray(log) ? (log as Marker[]) : [];
}

/** flag-context.mjs writes to its own key so its lines cannot mix with echo markers. */
function flagContextLog(): Marker[] {
	const key = "__leanFlagContextLog";
	const holder = globalThis as Record<string, unknown>;
	const log = holder[key];
	return Array.isArray(log) ? (log as Marker[]) : [];
}

afterEach(() => {
	(globalThis as Record<string, unknown>).__leanEchoLog = [];
	(globalThis as Record<string, unknown>).__leanFlagContextLog = [];
	delete (globalThis as Record<string, unknown>).__leanFlow;
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

	constructor(options: {
		cwd: string;
		extensionPaths: string[];
		beforeDeliver?: (frames: readonly Frame[]) => void | Promise<void>;
	}) {
		this.runner = new LeanRunner(this.stdin, {
			write: async (chunk: Uint8Array) => {
				const frames = this.decoder.push(chunk);
				await options.beforeDeliver?.(frames);
				for (const frame of frames) {
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

	allFrames(): readonly Frame[] {
		return this.frames;
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
			extensionPath: ECHO_ENTRY,
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

	test("rejects excluded imports through direct, minified, transitive, and cyclic graphs", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-import-graph-"));
		const direct = join(directory, "direct.mjs");
		const minified = join(directory, "minified.mjs");
		const transitive = join(directory, "transitive.mjs");
		const cyclic = join(directory, "cyclic-a.mjs");
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await Promise.all([
				writeFile(direct, 'import "jiti"; export default { name: "direct" };'),
				writeFile(minified, 'import{x}from"jiti";export default{name:"minified"};'),
				writeFile(transitive, 'import "./transitive-dependency.mjs"; export default { name: "transitive" };'),
				writeFile(
					join(directory, "transitive-dependency.mjs"),
					'export{x}from"jiti";',
				),
				writeFile(cyclic, 'import "./cyclic-b.mjs"; export default { name: "cyclic" };'),
				writeFile(
					join(directory, "cyclic-b.mjs"),
					'import "./cyclic-a.mjs"; import "typebox";',
				),
			]);
			await link.hello(1);
			link.request(2, "extensions.load", {
				extensionPaths: [ECHO_ENTRY, direct, minified, transitive, cyclic],
				cwd: directory,
			});
			const response = payload(await link.response(2, "extensions.load"));
			expect(response["extensions"]).toBe(1);
			const errors = new Map(
				(response["errors"] as Array<{ path: string; error: string }>).map(
					(error) => [error.path, error.error],
				),
			);
			expect(errors.get(direct)).toContain('excluded import "jiti"');
			expect(errors.get(minified)).toContain('excluded import "jiti"');
			expect(errors.get(transitive)).toContain('excluded import "jiti"');
			expect(errors.get(cyclic)).toContain('excluded import "typebox"');
		} finally {
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
	});

	test("checks literal require, rejects computed loads, and permits a shadowed require", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-opaque-import-"));
		const requireEntry = join(directory, "require.mjs");
		const computedEntry = join(directory, "computed.mjs");
		const shadowedEntry = join(directory, "shadowed.mjs");
		const holder = globalThis as Record<string, unknown>;
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await Promise.all([
				writeFile(
					requireEntry,
					'globalThis.__leanUnsupportedModuleLoadEvaluated = true; require("jiti"); export default { name: "require" };',
				),
				writeFile(
					computedEntry,
					'globalThis.__leanUnsupportedModuleLoadEvaluated = true; const name = "jiti"; await import(name); export default { name: "computed" };',
				),
				writeFile(
					shadowedEntry,
					'const require = (value) => value; export default { name: require("shadowed") };',
				),
			]);
			await link.hello(1);
			link.request(2, "extensions.load", {
				extensionPaths: [requireEntry, computedEntry, shadowedEntry],
				cwd: directory,
			});
			const response = payload(await link.response(2, "extensions.load"));
			const errors = new Map(
				(response["errors"] as Array<{ path: string; error: string }>).map(
					(error) => [error.path, error.error],
				),
			);
			expect(response["extensions"]).toBe(1);
			expect(errors.get(requireEntry)).toContain('excluded import "jiti"');
			expect(errors.get(computedEntry)).toContain("unsupported computed import(...)");
			expect(errors.has(shadowedEntry)).toBe(false);
			expect(holder["__leanUnsupportedModuleLoadEvaluated"]).toBeUndefined();
		} finally {
			delete holder["__leanUnsupportedModuleLoadEvaluated"];
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
	});

	test("rejects escaped bare require before extension evaluation", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-escaped-require-"));
		const escapedRequireEntry = join(directory, "escaped-require.mjs");
		const malformedRequireEntry = join(directory, "malformed-require.mjs");
		const holder = globalThis as Record<string, unknown>;
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await Promise.all([
				writeFile(
					escapedRequireEntry,
					'globalThis.__leanEscapedRequireEvaluated = true; \\u{72}equire("jiti"); export default { name: "escaped-require" };',
				),
				writeFile(
					malformedRequireEntry,
					'globalThis.__leanMalformedRequireEvaluated = true; \\uZZZZequire("jiti"); export default { name: "malformed-require" };',
				),
			]);
			await link.hello(1);
			link.request(2, "extensions.load", {
				extensionPaths: [escapedRequireEntry, malformedRequireEntry],
				cwd: directory,
			});
			const response = payload(await link.response(2, "extensions.load"));
			const errors = new Map(
				(response["errors"] as Array<{ path: string; error: string }>).map(
					(error) => [error.path, error.error],
				),
			);
			expect(response["extensions"]).toBe(0);
			expect(errors.get(escapedRequireEntry)).toContain('excluded import "jiti"');
			expect(errors.get(malformedRequireEntry)).toContain("unsupported malformed escaped identifier");
			expect(holder["__leanEscapedRequireEvaluated"]).toBeUndefined();
			expect(holder["__leanMalformedRequireEvaluated"]).toBeUndefined();
		} finally {
			delete holder["__leanEscapedRequireEvaluated"];
			delete holder["__leanMalformedRequireEvaluated"];
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
	});

	test("rejects createRequire and node:module loader bypasses before evaluation", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-create-require-"));
		const createRequireEntry = join(directory, "create-require.mjs");
		const nodeModuleEntry = join(directory, "node-module.mjs");
		const memberCreateRequireEntry = join(directory, "member-create-require.mjs");
		const bareCreateRequireEntry = join(directory, "bare-create-require.mjs");
		const holder = globalThis as Record<string, unknown>;
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await Promise.all([
				writeFile(
					createRequireEntry,
					'globalThis.__leanCreateRequireEvaluated = true; import { createRequire } from "node:module"; const load = createRequire(import.meta.url); load("jiti"); export default { name: "create-require" };',
				),
				writeFile(
					nodeModuleEntry,
					'globalThis.__leanNodeModuleEvaluated = true; import "module"; export default { name: "node-module" };',
				),
				writeFile(
					memberCreateRequireEntry,
					'globalThis.__leanMemberCreateRequireEvaluated = true; const r = module.createRequire; r(import.meta.url)("jiti"); export default { name: "member-create-require" };',
				),
				writeFile(
					bareCreateRequireEntry,
					'globalThis.__leanBareCreateRequireEvaluated = true; const load = createRequire(import.meta.url); load("jiti"); export default { name: "bare-create-require" };',
				),
			]);
			await link.hello(1);
			link.request(2, "extensions.load", {
				extensionPaths: [createRequireEntry, nodeModuleEntry, memberCreateRequireEntry, bareCreateRequireEntry],
				cwd: directory,
			});
			const response = payload(await link.response(2, "extensions.load"));
			const errors = new Map(
				(response["errors"] as Array<{ path: string; error: string }>).map(
					(error) => [error.path, error.error],
				),
			);
			// The createRequire name is caught as an unsupported loader form
			// on its import binding, before evaluation can begin.
			expect(errors.get(createRequireEntry)).toContain("unsupported createRequire loader");
			expect(errors.get(nodeModuleEntry)).toContain("module");
			// A bare createRequire call (no node:module import) is caught as
			// an unsupported loader form before evaluation.
			expect(errors.get(bareCreateRequireEntry)).toContain("unsupported createRequire loader");
			// A member-read createRequire alias (`const r = m.createRequire`)
			// fails closed as well: the loader name is rejected on ANY
			// appearance, so the aliased binding is never invoked.
			expect(errors.get(memberCreateRequireEntry)).toContain("unsupported createRequire loader");
			expect(holder["__leanCreateRequireEvaluated"]).toBeUndefined();
			expect(holder["__leanNodeModuleEvaluated"]).toBeUndefined();
			expect(holder["__leanMemberCreateRequireEvaluated"]).toBeUndefined();
			expect(holder["__leanBareCreateRequireEvaluated"]).toBeUndefined();
		} finally {
			delete holder["__leanCreateRequireEvaluated"];
			delete holder["__leanNodeModuleEvaluated"];
			delete holder["__leanMemberCreateRequireEvaluated"];
			delete holder["__leanBareCreateRequireEvaluated"];
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
	});
	test("rejects getBuiltinModule loader bypasses before evaluation", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-get-builtin-module-"));
		const bareEntry = join(directory, "get-builtin-module.mjs");
		const aliasedEntry = join(directory, "aliased-get-builtin-module.mjs");
		const escapedEntry = join(directory, "escaped-get-builtin-module.mjs");
		const memberEntry = join(directory, "member-get-builtin-module.mjs");
		const memberAliasEntry = join(directory, "member-alias-get-builtin-module.mjs");
		const computedEntry = join(directory, "computed-get-builtin-module.mjs");
		const cleanEntry = join(directory, "clean.mjs");
		const holder = globalThis as Record<string, unknown>;
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await Promise.all([
				writeFile(
					bareEntry,
					'globalThis.__leanGetBuiltinModuleEvaluated = true; const m = getBuiltinModule("module"); m.createRequire(import.meta.url)("jiti"); export default { name: "bare-get-builtin-module" };',
				),
				writeFile(
					aliasedEntry,
					'globalThis.__leanAliasedGetBuiltinModuleEvaluated = true; const { getBuiltinModule } = process; const m = getBuiltinModule("module"); m.createRequire(import.meta.url)("jiti"); export default { name: "aliased-get-builtin-module" };',
				),
				writeFile(
					escapedEntry,
					'globalThis.__leanEscapedGetBuiltinModuleEvaluated = true; const m = \\u{67}etBuiltinModule("module"); m.createRequire(import.meta.url)("jiti"); export default { name: "escaped-get-builtin-module" };',
				),
				writeFile(
					memberEntry,
					'globalThis.__leanMemberGetBuiltinModuleEvaluated = true; const m = process.getBuiltinModule("module"); m.createRequire(import.meta.url)("jiti"); export default { name: "member-get-builtin-module" };',
				),
				writeFile(
					memberAliasEntry,
					'globalThis.__leanMemberAliasGetBuiltinModuleEvaluated = true; const g = process.getBuiltinModule; g("module").createRequire(import.meta.url)("jiti"); export default { name: "member-alias-get-builtin-module" };',
				),
				writeFile(
					computedEntry,
					'globalThis.__leanComputedGetBuiltinModuleEvaluated = true; process["getBuiltinModule"]("module").createRequire(import.meta.url)("jiti"); export default { name: "computed-get-builtin-module" };',
				),
				writeFile(
					cleanEntry,
					'globalThis.__leanCleanEvaluated = true; export default { name: "clean" };',
				),
			]);
			await link.hello(1);
			link.request(2, "extensions.load", {
				extensionPaths: [bareEntry, aliasedEntry, escapedEntry, memberEntry, memberAliasEntry, computedEntry, cleanEntry],
				cwd: directory,
			});
			const response = payload(await link.response(2, "extensions.load"));
			const errors = new Map(
				(response["errors"] as Array<{ path: string; error: string }>).map(
					(error) => [error.path, error.error],
				),
			);
			// EVERY appearance of the loader name fails closed before
			// evaluation: bare call, destructured binding, Unicode-escaped
			// spelling, member call, member-read alias invoked through a local
			// binding, and computed string key.
			expect(errors.get(bareEntry)).toContain("unsupported getBuiltinModule loader");
			expect(errors.get(aliasedEntry)).toContain("unsupported getBuiltinModule loader");
			expect(errors.get(escapedEntry)).toContain("unsupported getBuiltinModule loader");
			expect(errors.get(memberEntry)).toContain("unsupported getBuiltinModule loader");
			expect(errors.get(memberAliasEntry)).toContain("unsupported getBuiltinModule loader");
			expect(errors.get(computedEntry)).toContain("unsupported getBuiltinModule loader");
			// Positive control: a clean extension in the same load batch still
			// evaluates and registers, proving the rejections are pre-evaluation
			// scan results rather than a wholesale load failure.
			expect(errors.has(cleanEntry)).toBe(false);
			expect(response["extensions"]).toBe(1);
			expect(holder["__leanCleanEvaluated"]).toBe(true);
			expect(holder["__leanGetBuiltinModuleEvaluated"]).toBeUndefined();
			expect(holder["__leanAliasedGetBuiltinModuleEvaluated"]).toBeUndefined();
			expect(holder["__leanEscapedGetBuiltinModuleEvaluated"]).toBeUndefined();
			expect(holder["__leanMemberGetBuiltinModuleEvaluated"]).toBeUndefined();
			expect(holder["__leanMemberAliasGetBuiltinModuleEvaluated"]).toBeUndefined();
			expect(holder["__leanComputedGetBuiltinModuleEvaluated"]).toBeUndefined();
		} finally {
			delete holder["__leanGetBuiltinModuleEvaluated"];
			delete holder["__leanAliasedGetBuiltinModuleEvaluated"];
			delete holder["__leanEscapedGetBuiltinModuleEvaluated"];
			delete holder["__leanMemberGetBuiltinModuleEvaluated"];
			delete holder["__leanMemberAliasGetBuiltinModuleEvaluated"];
			delete holder["__leanComputedGetBuiltinModuleEvaluated"];
			delete holder["__leanCleanEvaluated"];
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
	});

	test("rejects bare require reads (alias/destructured/computed) before evaluation", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-bare-require-"));
		const aliasEntry = join(directory, "alias.mjs");
		const destructuredEntry = join(directory, "destructured.mjs");
		const escapedEntry = join(directory, "escaped-bare.mjs");
		const computedEntry = join(directory, "computed-require.mjs");
		const shadowedEntry = join(directory, "shadowed.mjs");
		const cleanEntry = join(directory, "clean.mjs");
		const holder = globalThis as Record<string, unknown>;
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await Promise.all([
				// Plain assignment alias: `r("jiti")` never extracts a
				// specifier (r is not require), so only the bare read is
				// evidence of the bypass.
				writeFile(
					aliasEntry,
					'globalThis.__leanBareRequireAliasEvaluated = true; const r = require; r("jiti"); export default { name: "alias" };',
				),
				// Renamed destructured binding: the call uses the alias `r`,
				// so the specifier is not extracted; the bare `require` read
				// in the pattern is the only graph evidence.
				writeFile(
					destructuredEntry,
					'globalThis.__leanBareRequireDestructuredEvaluated = true; const { require: r } = module; r("jiti"); export default { name: "destructured" };',
				),
				// Escape-cooked bare read: `\u0072equire` cooks to `require`
				// via the cooked-identifier reader, then falls through to
				// the bare-read rejection.
				writeFile(
					escapedEntry,
					'globalThis.__leanBareRequireEscapedEvaluated = true; const r = \\u0072equire; r("jiti"); export default { name: "escaped-bare" };',
				),
				// Computed member key: `module["require"]` is caught by the
				// bracket-string reader, the same path as `["getBuiltinModule"]`.
				writeFile(
					computedEntry,
					'globalThis.__leanBareRequireComputedEvaluated = true; const r = module["require"]; r("jiti"); export default { name: "computed-require" };',
				),
				// Positive control for the declaration-keyword carve-out:
				// `const require = …` binds a fresh local that shadows the
				// global (the scanner cannot prove shadowing), so it must
				// still evaluate. The later `require("shadowed")` call
				// extracts the non-excluded specifier "shadowed".
				writeFile(
					shadowedEntry,
					'globalThis.__leanBareRequireShadowedEvaluated = true; const require = (value) => value; export default { name: require("shadowed") };',
				),
				// Positive control: a clean extension in the same batch still
				// evaluates and registers, proving the rejections are
				// pre-evaluation scan results rather than a wholesale failure.
				writeFile(
					cleanEntry,
					'globalThis.__leanBareRequireCleanEvaluated = true; export default { name: "clean" };',
				),
			]);
			await link.hello(1);
			link.request(2, "extensions.load", {
				extensionPaths: [aliasEntry, destructuredEntry, escapedEntry, computedEntry, shadowedEntry, cleanEntry],
				cwd: directory,
			});
			const response = payload(await link.response(2, "extensions.load"));
			const errors = new Map(
				(response["errors"] as Array<{ path: string; error: string }>).map(
					(error) => [error.path, error.error],
				),
			);
			// Every bare-require alias form fails closed before evaluation.
			expect(errors.get(aliasEntry)).toContain("unsupported bare require read");
			expect(errors.get(destructuredEntry)).toContain("unsupported bare require read");
			expect(errors.get(escapedEntry)).toContain("unsupported bare require read");
			expect(errors.get(computedEntry)).toContain("unsupported require loader");
			// The declaration-keyword carve-out keeps a shadowing `const
			// require = …` binding site permitted (not a bare read).
			expect(errors.has(shadowedEntry)).toBe(false);
			expect(holder["__leanBareRequireShadowedEvaluated"]).toBe(true);
			// Positive control: the clean extension still registers.
			expect(errors.has(cleanEntry)).toBe(false);
			expect(response["extensions"]).toBe(2);
			expect(holder["__leanBareRequireCleanEvaluated"]).toBe(true);
			// Rejected entries never set their evaluation markers.
			expect(holder["__leanBareRequireAliasEvaluated"]).toBeUndefined();
			expect(holder["__leanBareRequireDestructuredEvaluated"]).toBeUndefined();
			expect(holder["__leanBareRequireEscapedEvaluated"]).toBeUndefined();
			expect(holder["__leanBareRequireComputedEvaluated"]).toBeUndefined();
		} finally {
			delete holder["__leanBareRequireAliasEvaluated"];
			delete holder["__leanBareRequireDestructuredEvaluated"];
			delete holder["__leanBareRequireEscapedEvaluated"];
			delete holder["__leanBareRequireComputedEvaluated"];
			delete holder["__leanBareRequireShadowedEvaluated"];
			delete holder["__leanBareRequireCleanEvaluated"];
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
	});

	test("extensions.load of an already-loaded path registers nothing and does not double-count", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", { extensionPaths: [ECHO_ENTRY], cwd: PACKAGE_DIR });
		const first = payload(await link.response(2, "extensions.load"));
		expect(first["extensions"]).toBe(1);

		// A repeat load of the same path is a no-op: no duplicate shortcut,
		// no duplicate hook handler, and no inflated loaded count.
		link.request(3, "extensions.load", { extensionPaths: [ECHO_ENTRY], cwd: PACKAGE_DIR });
		const second = payload(await link.response(3, "extensions.load"));
		expect(second["extensions"]).toBe(0);
		expect(second["errors"]).toEqual([]);
		expect(second["shortcuts"]).toHaveLength(1);
		expect(link.runner.extensionCount).toBe(1);

		link.request(4, "session_start", { reason: "startup" });
		const res = payload(await link.response(4, "session_start"));
		expect(res["ok"]).toBe(true);
		const firings = markerLog().filter((marker) => marker.name === "hook.session_start");
		expect(firings).toHaveLength(1);
		await link.finish();
	});

	test("rejects malformed string escapes in import specifiers before evaluation", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-malformed-escape-"));
		const badHexEntry = join(directory, "bad-hex.mjs");
		const badBracedEntry = join(directory, "bad-braced.mjs");
		const surrogateEntry = join(directory, "surrogate.mjs");
		const cleanEntry = join(directory, "clean.mjs");
		const holder = globalThis as Record<string, unknown>;
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await Promise.all([
				writeFile(
					badHexEntry,
					'globalThis.__leanBadHexEvaluated = true; import "ji\\xNNti"; export default { name: "bad-hex" };',
				),
				writeFile(
					badBracedEntry,
					'globalThis.__leanBadBracedEvaluated = true; import "ji\\u{}ti"; export default { name: "bad-braced" };',
				),
				writeFile(
					surrogateEntry,
					'globalThis.__leanSurrogateEvaluated = true; import "ji\\u{D800}ti"; export default { name: "surrogate" };',
				),
				writeFile(
					cleanEntry,
					'globalThis.__leanMalformedCleanEvaluated = true; import "./d\\u0065p.mjs"; export default { name: "clean" };',
				),
				writeFile(join(directory, "dep.mjs"), "export const dep = true;"),
			]);
			await link.hello(1);
			link.request(2, "extensions.load", {
				extensionPaths: [badHexEntry, badBracedEntry, surrogateEntry, cleanEntry],
				cwd: directory,
			});
			const response = payload(await link.response(2, "extensions.load"));
			const errors = new Map(
				(response["errors"] as Array<{ path: string; error: string }>).map(
					(error) => [error.path, error.error],
				),
			);
			// Malformed hex/Unicode escapes fail closed through the scanner
			// instead of cooking to a wrong character that could hide a
			// forbidden specifier.
			expect(errors.get(badHexEntry)).toContain("unsupported malformed string escape");
			expect(errors.get(badBracedEntry)).toContain("unsupported malformed string escape");
			expect(errors.get(surrogateEntry)).toContain("unsupported malformed string escape");
			// A well-formed escape still cooks and evaluates normally.
			expect(errors.has(cleanEntry)).toBe(false);
			expect(holder["__leanMalformedCleanEvaluated"]).toBe(true);
			expect(holder["__leanBadHexEvaluated"]).toBeUndefined();
			expect(holder["__leanBadBracedEvaluated"]).toBeUndefined();
			expect(holder["__leanSurrogateEvaluated"]).toBeUndefined();
		} finally {
			delete holder["__leanBadHexEvaluated"];
			delete holder["__leanBadBracedEvaluated"];
			delete holder["__leanSurrogateEvaluated"];
			delete holder["__leanMalformedCleanEvaluated"];
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
	});

	test("finds excluded imports through Bun extensionless and directory-index local paths", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-extensionless-import-"));
		const extensionlessEntry = join(directory, "extensionless.mjs");
		const directoryEntry = join(directory, "directory.mjs");
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await mkdir(join(directory, "nested"));
			await Promise.all([
				writeFile(extensionlessEntry, 'import "./dep"; export default { name: "extensionless" };'),
				writeFile(join(directory, "dep.mjs"), 'import "jiti";'),
				writeFile(directoryEntry, 'import "./nested"; export default { name: "directory" };'),
				writeFile(join(directory, "nested", "index.mjs"), 'import "typebox";'),
			]);
			await link.hello(1);
			link.request(2, "extensions.load", {
				extensionPaths: [extensionlessEntry, directoryEntry],
				cwd: directory,
			});
			const response = payload(await link.response(2, "extensions.load"));
			const errors = new Map(
				(response["errors"] as Array<{ path: string; error: string }>).map(
					(error) => [error.path, error.error],
				),
			);
			expect(response["extensions"]).toBe(0);
			expect(errors.get(extensionlessEntry)).toContain('excluded import "jiti"');
			expect(errors.get(directoryEntry)).toContain('excluded import "typebox"');
		} finally {
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
	});

	test("isolates nested non-JSON metadata failures from valid extensions", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-metadata-"));
		const bigint = join(directory, "bigint.mjs");
		const undefinedValue = join(directory, "undefined.mjs");
		const infinity = join(directory, "infinity.mjs");
		const notANumber = join(directory, "nan.mjs");
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await Promise.all([
				writeFile(
					bigint,
					'export default { name: "bigint", tools: [{ name: "t", description: "d", parameters: { nested: { value: 1n } }, execute() {} }] };',
				),
				writeFile(
					undefinedValue,
					'export default { name: "undefined", providers: [{ name: "p", models: [{ nested: { value: undefined } }] }] };',
				),
				writeFile(
					infinity,
					'export default { name: "infinity", providers: [{ name: "p", models: [{ nested: { value: Infinity } }] }] };',
				),
				writeFile(
					notANumber,
					'export default { name: "nan", providers: [{ name: "p", models: [{ nested: { value: NaN } }] }] };',
				),
			]);
			await link.hello(1);
			link.request(2, "extensions.load", {
				extensionPaths: [ECHO_ENTRY, bigint, undefinedValue, infinity, notANumber],
				cwd: directory,
			});
			const response = payload(await link.response(2, "extensions.load"));
			expect(response["extensions"]).toBe(1);
			const errors = new Map(
				(response["errors"] as Array<{ path: string; error: string }>).map(
					(error) => [error.path, error.error],
				),
			);
			expect(errors.get(bigint)).toContain("BigInt");
			expect(errors.get(undefinedValue)).toContain("undefined");
			expect(errors.get(infinity)).toContain("finite JSON number");
			expect(errors.get(notANumber)).toContain("finite JSON number");
		} finally {
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
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
		expect(ok["args"]).toEqual({ text: "hi", validatedBy: "lean" });
		expect(markerLog()).toContainEqual({ name: "validate", value: { text: "hi" } });

		link.request(13, "tool.validate", { name: "echo", args: { text: 42 } });
		const err = payload(await link.error(13, "tool.validate"));
		expect(err["code"]).toBe("invalid_arguments");
		expect(String(err["message"])).toContain("echo.text must be a string");
		await link.finish();
	});

	test("tool.execute streams toolUpdate then resolves the tool result", async () => {
		const link = await loadedLink();
		link.request(13, "tool.validate", {
			name: "echo",
			args: { text: "hi", preparedBy: "lean" },
		});
		const validated = payload(await link.response(13, "tool.validate"));
		expect(validated["args"]).toEqual({
			text: "hi",
			preparedBy: "lean",
			validatedBy: "lean",
		});
		link.request(14, "tool.execute", {
			name: "echo",
			toolCallId: "call-1",
			args: validated["args"],
			prepared: true,
		});
		const update = await link.waitFor(
			(f) => f.id === 14 && f.kind === "event" && f.method === "toolUpdate",
		);
		expect(payload(update)).toMatchObject({ toolCallId: "call-1", toolName: "echo" });
		const res = payload(await link.response(14, "tool.execute"));
		expect(res["content"]).toEqual([{ type: "text", text: "echo:hi" }]);
		expect(res["details"]).toMatchObject({ preparedBy: "lean", extensionPath: ECHO_ENTRY });
		expect(markerLog()).toContainEqual({
			name: "execute",
			value: {
				args: { text: "hi", preparedBy: "lean", validatedBy: "lean" },
				toolCallId: "call-1",
				cwd: PACKAGE_DIR,
			},
		});
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
		// Synchronize on the tool's own started event before cancelling; the
		// separate test below covers an already-aborted signal.
		await link.waitFor(
			(f) => f.id === 15 && f.kind === "event" && f.method === "toolUpdate",
		);
		link.event("tool.cancel", { id: 15 });
		const err = payload(await link.error(15, "tool.execute"));
		expect(err["code"]).toBe("cancelled");
		await link.finish();
	});

	test("tool updates queue every value in arrival order and stop after terminal", async () => {
		const releaseFirstUpdate = Promise.withResolvers<void>();
		const firstUpdateStarted = Promise.withResolvers<void>();
		const late = Promise.withResolvers<void>();
		let updateWrites = 0;
		(globalThis as Record<string, unknown>).__leanFlow = { late };
		const link = new LeanLink({
			cwd: PACKAGE_DIR,
			extensionPaths: [FLOW_CONTROL_ENTRY],
			beforeDeliver: (frames) => {
				if (!frames.some((frame) => frame.kind === "event" && frame.method === "toolUpdate")) return;
				updateWrites++;
				if (updateWrites === 1) {
					firstUpdateStarted.resolve();
					return releaseFirstUpdate.promise;
				}
			},
		});
		await link.hello(1);
		link.request(2, "extensions.load", { extensionPaths: [FLOW_CONTROL_ENTRY], cwd: PACKAGE_DIR });
		await link.response(2, "extensions.load");
		link.request(3, "tool.execute", {
			name: "many-updates",
			toolCallId: "updates-1",
			args: {},
			prepared: true,
		});
		await firstUpdateStarted.promise;
		// Backpressure on the first update does not drop the rest: every
		// update is queued in arrival order, matching the full-host contract.
		expect(updateWrites).toBe(1);

		releaseFirstUpdate.resolve();
		await link.response(3, "tool.execute");
		const updates = link.allFrames().filter(
			(frame) => frame.id === 3 && frame.kind === "event" && frame.method === "toolUpdate",
		);
		const expected = Array.from({ length: 200 }, (_, index) => ({ index }));
		expect(updates.map((frame) => payload(frame)["partialResult"])).toEqual(expected);
		const terminalIndex = link.allFrames().findIndex(
			(frame) => frame.id === 3 && frame.kind === "res" && frame.method === "tool.execute",
		);
		expect(terminalIndex).toBeGreaterThan(link.allFrames().indexOf(updates[199] as Frame));

		late.resolve();
		await Promise.resolve();
		// The terminal result already stopped accepting updates, so the late
		// emission never enters the queue.
		expect(link.allFrames().filter(
			(frame) => frame.id === 3 && frame.kind === "event" && frame.method === "toolUpdate",
		)).toHaveLength(200);
		await link.finish();
	});

	test("tool cancellation stops accepting updates", async () => {
		const abortGate = Promise.withResolvers<void>();
		const releaseFirstUpdate = Promise.withResolvers<void>();
		const firstUpdateStarted = Promise.withResolvers<void>();
		let updateWrites = 0;
		(globalThis as Record<string, unknown>).__leanFlow = { abortGate };
		const link = new LeanLink({
			cwd: PACKAGE_DIR,
			extensionPaths: [FLOW_CONTROL_ENTRY],
			beforeDeliver: (frames) => {
				if (!frames.some((frame) => frame.kind === "event" && frame.method === "toolUpdate")) return;
				updateWrites++;
				if (updateWrites === 1) {
					firstUpdateStarted.resolve();
					return releaseFirstUpdate.promise;
				}
			},
		});
		await link.hello(1);
		link.request(2, "extensions.load", { extensionPaths: [FLOW_CONTROL_ENTRY], cwd: PACKAGE_DIR });
		await link.response(2, "extensions.load");
		link.request(3, "tool.execute", {
			name: "abort-updates",
			toolCallId: "abort-1",
			args: {},
			prepared: true,
		});
		await firstUpdateStarted.promise;
		link.event("tool.cancel", { id: 3 });

		// The barrier response is ordered after the cancel event. Releasing the
		// accepted update first lets it pass the paused write queue, then proves
		// cancellation reached the runner before the fixture emits again.
		link.request(4, "tool.prepare", { name: "abort-updates", args: {} });
		releaseFirstUpdate.resolve();
		await link.response(4, "tool.prepare");
		abortGate.resolve();
		const terminal = await link.waitFor(
			(frame) => frame.id === 3 && frame.method === "tool.execute"
				&& (frame.kind === "res" || frame.kind === "error"),
		);
		expect(terminal.kind).toBe("error");
		expect(payload(terminal)["code"]).toBe("cancelled");
		expect(updateWrites).toBe(1);
		expect(link.allFrames().filter(
			(frame) => frame.id === 3 && frame.kind === "event" && frame.method === "toolUpdate",
		).map((frame) => payload(frame)["partialResult"])).toEqual([{ index: "accepted" }]);
		await link.finish();
	});

	test("slow tool rejects when its signal was already aborted", async () => {
		// The fixture is selected by the absolute runtime path LeanRunner uses,
		// so this deliberately crosses the plugin-loading boundary.
		const fixture = (await import(ECHO_ENTRY)) as { default: LeanExtension };
		const slow = fixture.default.tools?.find((tool) => tool.name === "slow");
		if (slow === undefined) throw new Error("slow tool fixture is missing");

		const controller = new AbortController();
		controller.abort();
		let rejection: Error | undefined;
		const settled = await Promise.race([
			Promise.resolve(slow.execute({}, {
				cwd: PACKAGE_DIR,
				extensionPath: ECHO_ENTRY,
				flags: {},
				toolCallId: "already-aborted",
				signal: controller.signal,
				onUpdate: () => {},
			})).then(
				() => "fulfilled" as const,
				(error) => {
					rejection = error instanceof Error ? error : undefined;
					return "rejected" as const;
				},
			),
			Promise.resolve().then(() => "pending" as const),
		]);
		expect(settled).toBe("rejected");
		expect(rejection?.message).toBe("slow tool aborted");
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

	test("flags.set values reach callbacks through ctx.flags", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [FLAG_CONTEXT_ENTRY] });
		await link.hello(1);
		link.request(2, "extensions.load", { extensionPaths: [FLAG_CONTEXT_ENTRY], cwd: PACKAGE_DIR });
		await link.response(2, "extensions.load");

		// Before any flags.set, callbacks observe the declared defaults.
		link.request(3, "command.execute", { command: "report-flags", args: "" });
		await link.response(3, "command.execute");
		expect(flagContextLog()).toContainEqual({
			name: "flags",
			value: { mode: "quiet", debug: false },
		});

		// After flags.set, the applied values win over the defaults.
		link.request(4, "flags.set", { values: { mode: "loud", debug: true } });
		expect(payload(await link.response(4, "flags.set"))["ok"]).toBe(true);
		link.request(5, "command.execute", { command: "report-flags", args: "" });
		await link.response(5, "command.execute");
		expect(flagContextLog()).toContainEqual({
			name: "flags",
			value: { mode: "loud", debug: true },
		});

		// tool.execute contexts carry the same effective values.
		link.request(6, "tool.execute", { name: "flag-tool", toolCallId: "flag-1", args: {}, prepared: true });
		await link.response(6, "tool.execute");
		expect(flagContextLog()).toContainEqual({
			name: "tool-flags",
			value: { mode: "loud", debug: true },
		});
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

	test("shortcut single-flights matching keys while distinct keys overlap", async () => {
		const started = Promise.withResolvers<void>();
		const pending: Array<ReturnType<typeof Promise.withResolvers<void>>> = [];
		const seen: string[] = [];
		(globalThis as Record<string, unknown>).__leanFlow = {
			shortcut: (key: string) => {
				seen.push(key);
				if (seen.length === 2) started.resolve();
				const wait = Promise.withResolvers<void>();
				pending.push(wait);
				return wait.promise;
			},
		};
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [FLOW_CONTROL_ENTRY] });
		await link.hello(1);
		link.request(2, "extensions.load", { extensionPaths: [FLOW_CONTROL_ENTRY], cwd: PACKAGE_DIR });
		await link.response(2, "extensions.load");
		link.request(3, "shortcut.execute", { key: "ctrl+repeat" });
		link.request(4, "shortcut.execute", { key: "ctrl+repeat" });
		link.request(5, "shortcut.execute", { key: "ctrl+other" });
		await Promise.all([
			link.response(3, "shortcut.execute"),
			link.response(4, "shortcut.execute"),
			link.response(5, "shortcut.execute"),
		]);
		await started.promise;
		expect(seen.sort()).toEqual(["other", "repeat"]);
		for (const wait of pending) wait.resolve();
		await link.finish();
	});

	test("shortcut disposal aborts the active handler and clears its entry", async () => {
		const started = Promise.withResolvers<void>();
		const aborted = Promise.withResolvers<void>();
		(globalThis as Record<string, unknown>).__leanFlow = {
			shortcut: (_key: string, signal: AbortSignal) => {
				started.resolve();
				return new Promise<void>((resolve) => {
					signal.addEventListener("abort", () => {
						aborted.resolve();
						resolve();
					}, { once: true });
				});
			},
		};
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [FLOW_CONTROL_ENTRY] });
		await link.hello(1);
		link.request(2, "extensions.load", { extensionPaths: [FLOW_CONTROL_ENTRY], cwd: PACKAGE_DIR });
		await link.response(2, "extensions.load");
		link.request(3, "shortcut.execute", { key: "ctrl+repeat" });
		await link.response(3, "shortcut.execute");
		await started.promise;
		link.runner.dispose();
		await aborted.promise;
		expect((link.runner as unknown as { inFlightShortcuts: Map<string, AbortController> }).inFlightShortcuts.size).toBe(0);
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

	test("provider.stream honors provider.cancel with a cancelled error frame", async () => {
		const link = await loadedLink();
		// Sibling stream must keep completing while the slow one is cancelled.
		link.request(28, "provider.stream", {
			providerId: "lean-provider",
			model: { id: "slow" },
			context: {},
			options: {},
		});
		link.request(29, "provider.stream", {
			providerId: "lean-provider",
			model: { id: "m1" },
			context: {},
			options: {},
		});
		// Synchronize on the slow stream's start event: the AbortController is
		// registered before streamSimple runs, so the cancel cannot be lost.
		await link.waitFor(
			(f) => f.id === 28 && f.kind === "event" && f.method === "providerEvent",
		);
		link.event("provider.cancel", { id: 28 });
		const err = payload(await link.error(28, "provider.stream"));
		expect(err["code"]).toBe("cancelled");
		expect(err["message"]).toBe("provider stream cancelled");
		const sibling = payload(await link.response(29, "provider.stream"));
		expect(sibling).toEqual({});
		await link.finish();
	});

	test("provider.stream fails fast when the lifecycle signal is absent", async () => {
		// Dynamic import: exercises the real .mjs plugin-loading boundary.
		const fixture = (await import(ECHO_ENTRY)) as { default: LeanExtension };
		const provider = fixture.default.providers?.find((p) => p.name === "lean-provider");
		if (provider?.streamSimple === undefined) throw new Error("provider fixture is missing");

		const streamWithoutSignal = provider.streamSimple as (
			model: unknown,
			context: unknown,
			options: Record<string, unknown>,
		) => AsyncIterable<unknown>;
		const stream = streamWithoutSignal({ id: "slow" }, {}, {});
		await expect((async () => {
			for await (const _event of stream) { /* drain */ }
		})()).rejects.toThrow("provider stream requires options.signal");
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

	test("tool_call omits input when a no-op hook leaves arguments unchanged", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", {
			extensionPaths: [TOOL_CALL_NOOP_ENTRY],
			cwd: PACKAGE_DIR,
		});
		await link.response(2, "extensions.load");
		link.request(46, "tool_call", {
			toolName: "echo",
			toolCallId: "call-noop",
			input: { text: "untouched" },
		});
		const res = payload(await link.response(46, "tool_call"));
		expect(res).toEqual({ block: false, reason: "noop-ack" });
		expect(Object.hasOwn(res, "input")).toBe(false);
		await link.finish();
	});


	test("tool_call omits input when a hook only reorders object keys", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", {
			extensionPaths: [TOOL_CALL_REORDER_ENTRY],
			cwd: PACKAGE_DIR,
		});
		await link.response(2, "extensions.load");
		link.request(47, "tool_call", {
			toolName: "echo",
			toolCallId: "call-reorder",
			input: { a: 1, m: 2, z: 3 },
		});
		const res = payload(await link.response(47, "tool_call"));
		expect(res).toEqual({ block: false, reason: "reorder-ack" });
		expect(Object.hasOwn(res, "input")).toBe(false);
		await link.finish();
	});

	test("tool_call includes input when a hook changes a value", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", {
			extensionPaths: [TOOL_CALL_VALUE_CHANGE_ENTRY],
			cwd: PACKAGE_DIR,
		});
		await link.response(2, "extensions.load");
		link.request(48, "tool_call", {
			toolName: "echo",
			toolCallId: "call-value",
			input: { a: "original", m: 2, z: 3 },
		});
		const res = payload(await link.response(48, "tool_call"));
		expect(res["block"]).toBe(false);
		expect(res["reason"]).toBe("value-ack");
		expect(res["input"]).toEqual({ a: "changed", m: 2, z: 3 });
		await link.finish();
	});

	test("input detects in-place image mutations but ignores JSON-equivalent images", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-images-"));
		const unchanged = join(directory, "unchanged.mjs");
		const reordered = join(directory, "reordered.mjs");
		const mutated = join(directory, "mutated.mjs");
		try {
			await Promise.all([
				writeFile(
					unchanged,
					'export default { name: "unchanged", hooks: { input: (event) => ({ action: "transform", text: event.text }) } };',
				),
				writeFile(
					reordered,
					'export default { name: "reordered", hooks: { input(event) { const image = event.images[0]; const entries = Object.entries(image).reverse(); for (const key of Object.keys(image)) delete image[key]; for (const [key, value] of entries) image[key] = value; return { action: "transform", text: event.text }; } } };',
				),
				writeFile(
					mutated,
					'export default { name: "mutated", hooks: { input(event) { event.images[0].metadata.state = "changed"; return { action: "transform", text: event.text }; } } };',
				),
			]);
			const cases = [
				{ entry: unchanged, expected: { action: "continue" } },
				{ entry: reordered, expected: { action: "continue" } },
				{
					entry: mutated,
					expected: {
						action: "transform",
						text: "image",
						images: [{ id: "one", metadata: { state: "changed", tags: ["keep"] } }],
					},
				},
			] as const;
			for (const [index, entry] of cases.entries()) {
				const link = new LeanLink({ cwd: directory, extensionPaths: [] });
				try {
					await link.hello(1);
					link.request(2, "extensions.load", { extensionPaths: [entry.entry], cwd: directory });
					await link.response(2, "extensions.load");
					const id = 50 + index;
					link.request(id, "input", {
						text: "image",
						images: [{ id: "one", metadata: { state: "base", tags: ["keep"] } }],
						source: "interactive",
					});
					expect(payload(await link.response(id, "input"))).toEqual(entry.expected);
				} finally {
					await link.finish();
				}
			}
		} finally {
			await rm(directory, { recursive: true, force: true });
		}
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

	test("message_update CancelWire is forwarded; non-cancel keeps ok: true", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", {
			extensionPaths: [MESSAGE_UPDATE_CANCEL_ENTRY],
			cwd: PACKAGE_DIR,
		});
		await link.response(2, "extensions.load");

		// Void / non-cancel hook result → Mode 1 shape `{ ok: true }`.
		link.request(37, "message_update_delta", {
			type: "message_update_delta",
			event: { type: "start", meta: { role: "assistant" } },
		});
		const started = payload(await link.response(37, "message_update_delta"));
		expect(started).toEqual({ ok: true });

		// `{ cancel: false }` is a non-cancel result and keeps `{ ok: true }`.
		link.request(38, "message_update_delta", {
			type: "message_update_delta",
			event: { type: "text_delta", meta: {}, contentIndex: 0, delta: "hi" },
		});
		const nonCancel = payload(await link.response(38, "message_update_delta"));
		expect(nonCancel).toEqual({ ok: true });

		// `{ cancel: true, reason }` must reach the wire so Rust sees CancelWire.
		link.request(39, "message_update_delta", {
			type: "message_update_delta",
			event: { type: "text_delta", meta: {}, contentIndex: 0, delta: "veto" },
		});
		const cancelled = payload(await link.response(39, "message_update_delta"));
		expect(cancelled).toEqual({ cancel: true, reason: "stop-from-lean" });
		await link.finish();
	});
});























// ---------------------------------------------------------------------------
// AssistantDeltaReducer: contentIndex bound at the extension wire boundary
// ---------------------------------------------------------------------------

describe("lean: AssistantDeltaReducer contentIndex bound", () => {
	/** Reducer primed with a `start` event so deltas apply against an empty content array. */
	function freshReducer(): AssistantDeltaReducer {
		const reducer = new AssistantDeltaReducer();
		reducer.applyAssistantDelta({ type: "start", meta: { role: "assistant" } });
		return reducer;
	}

	/** Active content array (empty when no assistant is seeded), typed for index/key checks. */
	function contentOf(reducer: AssistantDeltaReducer): unknown[] {
		const message = reducer.getActiveAssistant() as Record<string, unknown> | undefined;
		const content = message?.["content"];
		return Array.isArray(content) ? (content as unknown[]) : [];
	}

	/** Apply a block event at the given contentIndex against a fresh reducer. */
	function startBlock(index: unknown, type: string, block: Record<string, unknown>): unknown[] {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({ type, meta: {}, contentIndex: index, block });
		return contentOf(reducer);
	}

	test("append at contentIndex === content.length is accepted for *_start", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "" },
		});
		expect(contentOf(reducer)).toEqual([{ type: "text", text: "" }]);
	});

	test("replace at an existing contentIndex keeps block replacement working", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "draft" },
		});
		reducer.applyAssistantDelta({
			type: "text_end",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "final" },
		});
		expect(contentOf(reducer)).toEqual([{ type: "text", text: "final" }]);
	});

	test("streamed text deltas still accumulate at a valid index", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "" },
		});
		reducer.applyAssistantDelta({ type: "text_delta", meta: {}, contentIndex: 0, delta: "hel" });
		reducer.applyAssistantDelta({ type: "text_delta", meta: {}, contentIndex: 0, delta: "lo" });
		expect(contentOf(reducer)).toEqual([{ type: "text", text: "hello" }]);
	});

	test("tool-call argument tracking still works at a valid index", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 0,
			block: { type: "toolCall", id: "c1", name: "read", arguments: {} },
		});
		reducer.applyAssistantDelta({
			type: "toolcall_delta",
			meta: {},
			contentIndex: 0,
			delta: '{"path":"README',
		});
		reducer.applyAssistantDelta({
			type: "toolcall_delta",
			meta: {},
			contentIndex: 0,
			delta: '.md"}',
		});
		expect(contentOf(reducer)).toEqual([
			{ type: "toolCall", id: "c1", name: "read", arguments: { path: "README.md" } },
		]);
	});

	test("ignores a negative contentIndex instead of polluting the array", () => {
		const content = startBlock(-1, "text_start", { type: "text", text: "" });
		expect(content).toHaveLength(0);
		// A negative index would attach a "-1" string key, not an element.
		expect(Object.keys(content)).toEqual([]);
	});

	test("ignores a fractional contentIndex", () => {
		const content = startBlock(0.5, "text_start", { type: "text", text: "" });
		expect(content).toHaveLength(0);
		expect(Object.keys(content)).toEqual([]);
	});

	test("ignores a NaN contentIndex", () => {
		const content = startBlock(Number.NaN, "text_start", { type: "text", text: "" });
		expect(content).toHaveLength(0);
		expect(Object.keys(content)).toEqual([]);
	});

	test("ignores an Infinity contentIndex", () => {
		const content = startBlock(Number.POSITIVE_INFINITY, "text_start", { type: "text", text: "" });
		expect(content).toHaveLength(0);
		expect(Object.keys(content)).toEqual([]);
	});

	test("ignores a -Infinity contentIndex", () => {
		const content = startBlock(Number.NEGATIVE_INFINITY, "text_start", { type: "text", text: "" });
		expect(content).toHaveLength(0);
		expect(Object.keys(content)).toEqual([]);
	});

	test("ignores a contentIndex far past the end", () => {
		const content = startBlock(5, "text_start", { type: "text", text: "" });
		expect(content).toHaveLength(0);
		expect(Object.keys(content)).toEqual([]);
	});

	test("ignores an out-of-range toolcall_start so the tool-argument map stays clean", () => {
		// A toolcall_start at a far index would otherwise both append a sparse
		// block and seed the activeToolArguments map for that bogus index.
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 9,
			block: { type: "toolCall", id: "c9", name: "read", arguments: {} },
		});
		const content = contentOf(reducer);
		expect(content).toHaveLength(0);
		expect(Object.keys(content)).toEqual([]);
		// A follow-up delta at the bogus index must find no seeded block.
		reducer.applyAssistantDelta({
			type: "toolcall_delta",
			meta: {},
			contentIndex: 9,
			delta: '{"evil":true}',
		});
		expect(contentOf(reducer)).toEqual([]);
	});

	test("ignores a non-number contentIndex (string index still dropped)", () => {
		const content = startBlock("0", "text_start", { type: "text", text: "" });
		expect(content).toHaveLength(0);
		expect(Object.keys(content)).toEqual([]);
	});

	test("text_end at contentIndex === content.length is ignored and appends nothing", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "hi" },
		});
		// content.length is now 1; an end at the append index must not fabricate a block.
		reducer.applyAssistantDelta({
			type: "text_end",
			meta: {},
			contentIndex: 1,
			block: { type: "text", text: "phantom" },
		});
		expect(contentOf(reducer)).toEqual([{ type: "text", text: "hi" }]);
	});

	test("thinking_end at contentIndex === content.length is ignored and appends nothing", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "thinking_start",
			meta: {},
			contentIndex: 0,
			block: { type: "thinking", thinking: "hi" },
		});
		reducer.applyAssistantDelta({
			type: "thinking_end",
			meta: {},
			contentIndex: 1,
			block: { type: "thinking", thinking: "phantom" },
		});
		expect(contentOf(reducer)).toEqual([{ type: "thinking", thinking: "hi" }]);
	});

	test("toolcall_end at contentIndex === content.length is ignored and appends nothing", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 0,
			block: { type: "toolCall", id: "c1", name: "read", arguments: {} },
		});
		reducer.applyAssistantDelta({
			type: "toolcall_end",
			meta: {},
			contentIndex: 1,
			block: { type: "toolCall", id: "phantom", name: "read", arguments: {} },
		});
		expect(contentOf(reducer)).toEqual([
			{ type: "toolCall", id: "c1", name: "read", arguments: {} },
		]);
	});

	test("text_start at contentIndex === content.length still appends normally", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "first" },
		});
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 1,
			block: { type: "text", text: "second" },
		});
		expect(contentOf(reducer)).toEqual([
			{ type: "text", text: "first" },
			{ type: "text", text: "second" },
		]);
	});

	test("thinking_start at contentIndex === content.length still appends normally", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "thinking_start",
			meta: {},
			contentIndex: 0,
			block: { type: "thinking", thinking: "first" },
		});
		reducer.applyAssistantDelta({
			type: "thinking_start",
			meta: {},
			contentIndex: 1,
			block: { type: "thinking", thinking: "second" },
		});
		expect(contentOf(reducer)).toEqual([
			{ type: "thinking", thinking: "first" },
			{ type: "thinking", thinking: "second" },
		]);
	});

	test("toolcall_start at contentIndex === content.length still appends normally", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 0,
			block: { type: "toolCall", id: "c1", name: "read", arguments: {} },
		});
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 1,
			block: { type: "toolCall", id: "c2", name: "write", arguments: {} },
		});
		expect(contentOf(reducer)).toEqual([
			{ type: "toolCall", id: "c1", name: "read", arguments: {} },
			{ type: "toolCall", id: "c2", name: "write", arguments: {} },
		]);
	});

	test("text_end at an existing contentIndex still replaces the block", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "draft" },
		});
		reducer.applyAssistantDelta({
			type: "text_end",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "final" },
		});
		expect(contentOf(reducer)).toEqual([{ type: "text", text: "final" }]);
	});

	test("toolcall_end at an existing contentIndex replaces the block and clears tracked arguments", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 0,
			block: { type: "toolCall", id: "c1", name: "read", arguments: {} },
		});
		reducer.applyAssistantDelta({
			type: "toolcall_delta",
			meta: {},
			contentIndex: 0,
			delta: '{"path":"a.md"}',
		});
		// Pre-end: arguments reflect the accumulated fragment.
		expect((contentOf(reducer)[0] as Record<string, unknown>)["arguments"]).toEqual({
			path: "a.md",
		});
		reducer.applyAssistantDelta({
			type: "toolcall_end",
			meta: {},
			contentIndex: 0,
			block: { type: "toolCall", id: "c1", name: "read", arguments: {} },
		});
		// The end block replaces the streamed block in place.
		expect(contentOf(reducer)).toEqual([
			{ type: "toolCall", id: "c1", name: "read", arguments: {} },
		]);
		// Tracked arguments were cleared: a follow-up delta starts fresh rather
		// than concatenating with the pre-end fragment (which would reparse to
		// { path: "a.md" } instead of { path: "b.md" }).
		reducer.applyAssistantDelta({
			type: "toolcall_delta",
			meta: {},
			contentIndex: 0,
			delta: '{"path":"b.md"}',
		});
		expect((contentOf(reducer)[0] as Record<string, unknown>)["arguments"]).toEqual({
			path: "b.md",
		});
	});

	test("text_start below content.length is rejected (no overwrite)", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "original" },
		});
		// A duplicate start at the same index must not replace the existing block.
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "overwritten" },
		});
		expect(contentOf(reducer)).toEqual([{ type: "text", text: "original" }]);
	});

	test("thinking_start below content.length is rejected (no overwrite)", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "thinking_start",
			meta: {},
			contentIndex: 0,
			block: { type: "thinking", thinking: "original" },
		});
		reducer.applyAssistantDelta({
			type: "thinking_start",
			meta: {},
			contentIndex: 0,
			block: { type: "thinking", thinking: "overwritten" },
		});
		expect(contentOf(reducer)).toEqual([{ type: "thinking", thinking: "original" }]);
	});

	test("toolcall_start below content.length is rejected and argument map stays clean", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 0,
			block: { type: "toolCall", id: "c1", name: "read", arguments: {} },
		});
		reducer.applyAssistantDelta({
			type: "toolcall_delta",
			meta: {},
			contentIndex: 0,
			delta: '{"path":"a.md"}',
		});
		// A duplicate toolcall_start at the occupied index must not reset the block
		// or clear the accumulated argument fragments.
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 0,
			block: { type: "toolCall", id: "c1-dup", name: "write", arguments: {} },
		});
		const block = contentOf(reducer)[0] as Record<string, unknown>;
		expect(block["id"]).toBe("c1");
		expect(block["name"]).toBe("read");
		// Argument tracking survived: the next delta concatenates, not restarts.
		reducer.applyAssistantDelta({
			type: "toolcall_delta",
			meta: {},
			contentIndex: 0,
			delta: "",
		});
		expect((contentOf(reducer)[0] as Record<string, unknown>)["arguments"]).toEqual({
			path: "a.md",
		});
	});

	test("text_start above content.length is rejected (no gap)", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 0,
			block: { type: "text", text: "first" },
		});
		// Skipping index 1 to append at 2 must leave the array unchanged.
		reducer.applyAssistantDelta({
			type: "text_start",
			meta: {},
			contentIndex: 2,
			block: { type: "text", text: "gap" },
		});
		expect(contentOf(reducer)).toEqual([{ type: "text", text: "first" }]);
	});

	test("thinking_start above content.length is rejected (no gap)", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "thinking_start",
			meta: {},
			contentIndex: 0,
			block: { type: "thinking", thinking: "first" },
		});
		reducer.applyAssistantDelta({
			type: "thinking_start",
			meta: {},
			contentIndex: 2,
			block: { type: "thinking", thinking: "gap" },
		});
		expect(contentOf(reducer)).toEqual([{ type: "thinking", thinking: "first" }]);
	});

	test("toolcall_start above content.length is rejected (no gap)", () => {
		const reducer = freshReducer();
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 0,
			block: { type: "toolCall", id: "c1", name: "read", arguments: {} },
		});
		reducer.applyAssistantDelta({
			type: "toolcall_start",
			meta: {},
			contentIndex: 2,
			block: { type: "toolCall", id: "c2", name: "write", arguments: {} },
		});
		expect(contentOf(reducer)).toEqual([
			{ type: "toolCall", id: "c1", name: "read", arguments: {} },
		]);
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


	test("tool_result rejects malformed input before dispatching hooks", async () => {
		const link = await foldLink();
		try {
			const malformed = [undefined, null, [], "raw"] as const;
			for (const [index, input] of malformed.entries()) {
				const requestPayload: Record<string, unknown> = {
					toolName: "echo",
					toolCallId: `malformed-${index}`,
					content: [],
					details: {},
					isError: false,
				};
				if (input !== undefined) requestPayload["input"] = input;
				const id = 60 + index;
				link.request(id, "tool_result", requestPayload);
				const error = payload(await link.error(id, "tool_result"));
				expect(error["code"]).toBe("extension_error");
				expect(String(error["message"])).toContain("tool_result.input is required");
			}
			expect(markerLog().some((entry) => entry.name.endsWith(".tool_result"))).toBe(false);
		} finally {
			await link.finish();
		}
	});

	test("message_end: raw payload message folds ordered replacements", async () => {
		const link = await foldLink();
		// Rust sends the raw AgentMessage as the payload, not { message }.
		link.request(43, "message_end", { role: "assistant", content: "base" });
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

	test("message_end: a wrong-role replacement emits an extension error and is ignored", async () => {
		const link = new LeanLink({ cwd: PACKAGE_DIR, extensionPaths: [] });
		await link.hello(1);
		link.request(2, "extensions.load", {
			extensionPaths: [ROLE_BREAKER_ENTRY],
			cwd: PACKAGE_DIR,
		});
		await link.response(2, "extensions.load");
		link.request(44, "message_end", { role: "assistant", content: "base" });
		const res = payload(await link.response(44, "message_end"));
		expect(res["message"]).toBeUndefined();
		const errEvent = await link.waitFor(
			(f) => f.kind === "event" && f.method === "extensionError",
		);
		expect(String(payload(errEvent)["message"])).toContain("same role");
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

	test("parseLeanExtension rejects malformed runtime surfaces", () => {
		const cases = [
			[
				{ flags: [{ name: "f", type: "boolean", default: "true" }] },
				"default must be a boolean",
			],
			[
				{ flags: [{ name: "f", type: "string", default: true }] },
				"default must be a string",
			],
			[{ tools: [{ name: "", description: "d", execute: () => ({}) }] }, "name must be a non-empty string"],
			[{ commands: [{ name: "", handler: () => {} }] }, "name must be a non-empty string"],
			[{ providers: [{ name: "" }] }, "name must be a non-empty string"],
			[{ tools: [{ name: "t", description: "d", execute: true }] }, "execute must be a function"],
			[{ commands: [{ name: "c", handler: true }] }, "handler must be a function"],
			[{ shortcuts: [{ key: "ctrl+x", handler: true }] }, "handler must be a function"],
		] as const;

		for (const [definition, message] of cases) {
			expect(() => parseLeanExtension(definition)).toThrow(message);
		}
	});

	test("parseLeanExtension accepts optional and correctly typed flag defaults", () => {
		for (const flag of [
			{ name: "bool-omitted", type: "boolean" },
			{ name: "string-omitted", type: "string" },
			{ name: "bool-default", type: "boolean", default: false },
			{ name: "string-default", type: "string", default: "" },
		]) {
			expect(() => parseLeanExtension({ flags: [flag] })).not.toThrow();
		}
	});

	test("parseLeanExtension rejects empty optional strings like required ones", () => {
		// requireString and optionalString agree: the empty string is not a
		// value, so an empty label/description can never reach the registry
		// snapshot (`label ?? name` only falls back on nullish).
		const cases: Array<[definition: unknown, message: string]> = [
			[{ tools: [{ name: "t", description: "d", label: "", execute: () => ({}) }] }, "label must be a non-empty string when present"],
			[{ commands: [{ name: "c", description: "", handler: () => {} }] }, "description must be a non-empty string when present"],
			[{ flags: [{ name: "f", type: "boolean", description: "" }] }, "description must be a non-empty string when present"],
			[{ shortcuts: [{ key: "ctrl+x", description: "", handler: () => {} }] }, "description must be a non-empty string when present"],
			[{ providers: [{ name: "p", displayName: "" }] }, "displayName must be a non-empty string when present"],
			[{ providers: [{ name: "p", baseUrl: "" }] }, "baseUrl must be a non-empty string when present"],
			[{ providers: [{ name: "p", api: "" }] }, "api must be a non-empty string when present"],
			[{ providers: [{ name: "p", apiKey: "" }] }, "apiKey must be a non-empty string when present"],
		];
		for (const [definition, message] of cases) {
			expect(() => parseLeanExtension(definition)).toThrow(message);
		}
		// Absent optionals stay optional.
		expect(() =>
			parseLeanExtension({
				tools: [{ name: "t", description: "d", execute: () => ({}) }],
				commands: [{ name: "c", handler: () => {} }],
			})
		).not.toThrow();
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

	test("parseLeanExtension validates executionMode before the snapshot", () => {
		const tool = { name: "t", description: "d", execute: () => ({}) };
		expect(() => parseLeanExtension({ tools: [tool] })).not.toThrow();
		expect(() =>
			parseLeanExtension({ tools: [{ ...tool, executionMode: "sequential" }] })
		).not.toThrow();
		expect(() =>
			parseLeanExtension({ tools: [{ ...tool, executionMode: "parallel" }] })
		).not.toThrow();
		expect(() =>
			parseLeanExtension({ tools: [{ ...tool, executionMode: "serial" }] })
		).toThrow('executionMode must be "sequential" or "parallel"');
	});

	test("parseLeanExtension rejects nested non-JSON tool and provider metadata", () => {
		const cycle: Record<string, unknown> = {};
		cycle["self"] = cycle;
		const invalidDefinitions: Array<[definition: unknown, message: string]> = [
			[
				{
					tools: [{
						name: "t",
						description: "d",
						parameters: { properties: { nested: { const: 1n } } },
						execute: () => ({}),
					}],
				},
				"BigInt",
			],
			[
				{
					tools: [{
						name: "t",
						description: "d",
						parameters: { properties: { nested: { default: undefined } } },
						execute: () => ({}),
					}],
				},
				"undefined",
			],
			[
				{ providers: [{ name: "p", models: [{ metadata: { score: Infinity } }] }] },
				"finite JSON number",
			],
			[
				{ providers: [{ name: "p", models: [{ metadata: { score: Number.NaN } }] }] },
				"finite JSON number",
			],
			[
				{ providers: [{ name: "p", models: [{ metadata: { callback: () => {} } }] }] },
				"function",
			],
			[
				{ providers: [{ name: "p", models: [{ metadata: { marker: Symbol("x") } }] }] },
				"symbol",
			],
			[
				{ providers: [{ name: "p", models: [cycle] }] },
				"cycle",
			],
		];

		for (const [definition, message] of invalidDefinitions) {
			expect(() => parseLeanExtension(definition)).toThrow(message);
		}
	});

	test("findExcludedImport detects the compat graph and tolerates clean code", () => {
		// One table witnesses T18 (minified/export forms) and
		// RB14-import-duplicate (ONE scanner covers every form).
		const cases: Array<[source: string, expected: string | undefined]> = [
			// Static import, spaced and minified.
			['import { x } from "@earendil-works/pi-coding-agent/builtins";', "@earendil-works/pi-coding-agent/builtins"],
			['import{x}from"@earendil-works/pi-coding-agent/builtins";', "@earendil-works/pi-coding-agent/builtins"],
			// Named and star re-export, spaced and minified.
			['export { x } from "jiti";', "jiti"],
			['export{x}from"jiti";', "jiti"],
			['export*from"typebox";', "typebox"],
			['export * from "typebox";', "typebox"],
			// Dynamic import and side-effect import (spaced and minified).
			['const m = await import("jiti");', "jiti"],
			['import "./host.ts";', "./host.ts"],
			// node:module loader factory is excluded: createRequire bypasses the graph.
			['import { createRequire } from "node:module";', "node:module"],
			['import "module";', "module"],
			['import { createRequire } from "node:module"; const load = createRequire(import.meta.url);', "node:module"],
			// Clean graph and keyword-shaped traps stay undetected.
			['import { y } from "@earendil-works/pi-tui-protocol";', undefined],
			['import{y}from"@earendil-works/pi-tui-protocol";', undefined],
			["export default { name: 'clean' };", undefined],
			["const url = import.meta.url;", undefined],
			['const important = 1; const exporter = 2; const imports = ["jiti"];', undefined],
			['obj.import("jiti"); deimport("jiti");', undefined],
		];
		for (const [source, expected] of cases) {
			expect(findExcludedImport(source)).toBe(expected);
		}
	});

	test("findExcludedImport decodes escaped dynamic-import specifiers", () => {
		// JavaScript cooks string-literal escapes before evaluating the
		// specifier, so `import("j\u0069ti")` loads `jiti`. The scanner must
		// decode the same escapes or the exclusion check sees raw source text.
		const cases: Array<[source: string, expected: string | undefined]> = [
			// Unicode escapes, four-digit and braced forms.
			['const m = await import("j\\u0069ti");', "jiti"],
			['const m = await import("j\\u{69}ti");', "jiti"],
			// Hex escapes and a backslash-escaped quote inside the literal.
			["const m = await import('\\x6aiti');", "jiti"],
			['import "@earendil-works\\/pi-coding-agent";', "@earendil-works/pi-coding-agent"],
			// A clean specifier with escapes stays undetected once decoded.
			['const m = await import("./cl\\u0065an.ts");', undefined],
		];
		for (const [source, expected] of cases) {
			expect(findExcludedImport(source)).toBe(expected);
		}
	});

	test("findExcludedImport cooks escaped loader identifiers without mistaking lexical non-loads", () => {
		const cases: Array<[source: string, expected: string | undefined]> = [
			['\\u0072equire("jiti");', "jiti"],
			['\\u{72}equire("jiti");', "jiti"],
			['\\u0069mport("jiti");', "jiti"],
			['import { x } \\u0066rom "jiti";', "jiti"],
			['ordinaryRequire("jiti");', undefined],
			['module.require("jiti");', undefined],
			['module.\\u0072equire("jiti");', undefined],
			['this.#\\u0072equire("jiti");', undefined],
			['const π\\u0072equire = (value) => value; π\\u0072equire("jiti");', undefined],
			['obj?.\\u006Ff / require("jiti") / divisor;', "jiti"],
			['// \\u0072equire("jiti");', undefined],
		];
		for (const [source, expected] of cases) {
			expect(findExcludedImport(source)).toBe(expected);
		}
	});

	test("excluded-specifier and preload guards agree on aliased typebox and anchored pi-ai", () => {
		// The preload fixture executes a Bun plugin on import, so its regex is
		// read back from source instead of imported; the table then proves the
		// two guards cannot drift apart.
		const fixtureSource = readFileSync(PRELOAD, "utf8");
		const forbiddenMatch = fixtureSource.match(/const FORBIDDEN =\s*\/(.+)\/;/);
		expect(forbiddenMatch).not.toBeNull();
		const forbidden = new RegExp(forbiddenMatch?.[1] ?? "");
		const cases: Array<[specifier: string, excluded: boolean]> = [
			["typebox", true],
			["typebox/value", true],
			["@sinclair/typebox", true],
			["@sinclair/typebox/value", true],
			["@earendil-works/pi-ai", true],
			["@earendil-works/pi-ai/compat", true],
			["@earendil-works/pi-ai-extra", false],
			["@earendil-works/pi-tui-protocol", false],
			["jiti", true],
			["jiti/subpath", true],
			["some-package", false],
		];
		for (const [specifier, excluded] of cases) {
			expect(findExcludedImport(`import "${specifier}";`)).toBe(excluded ? specifier : undefined);
			expect(forbidden.test(specifier)).toBe(excluded);
		}
	});

	test("findExcludedImport advances through an unterminated braced Unicode escape", () => {
		expect(findExcludedImport('import "j\\u{";')).toBeUndefined();
	});

	test("parseStreamingJson tolerates truncated streams", () => {
		expect(parseStreamingJson('{"a":1,"b":[true,"x"]}')).toEqual({ a: 1, b: [true, "x"] });
		// Close-and-trim best effort: a truncated array closes empty rather
		// than dropping the key (documented tolerant behavior).
		expect(parseStreamingJson('{"a":1,"b":[tru')).toEqual({ a: 1, b: [] });
		expect(parseStreamingJson('{"a":"hel')).toEqual({ a: "hel" });
		expect(parseStreamingJson('{"a":truex')).toEqual({ a: true });
		expect(parseStreamingJson("garbage")).toEqual({});
		expect(parseStreamingJson(undefined)).toEqual({});
	});

	test("lexical import scanner separates code from comments, strings, and template text", () => {
		const cases: Array<[source: string, expected: string | undefined]> = [
			// Commented-out imports of every form are not imports.
			['// import "jiti";', undefined],
			['// const m = await import("jiti");', undefined],
			['/* import { x } from "jiti"; */', undefined],
			['/* export * from "typebox"; */', undefined],
			// Import-shaped text inside strings and template raw text is inert.
			["const s = 'import \"jiti\"';", undefined],
			['const s = "export { x } from \'typebox\'";', undefined],
			['const s = `import "jiti" from "typebox"`;', undefined],
			['const s = `a ${ `b import "jiti" b` } c`;', undefined],
			// Escaped template characters stay raw text.
			['const s = `\\${ import("jiti") }`;', undefined],
			// Dynamic imports may carry comments between their tokens.
			['await import(/* lazy */ "jiti");', "jiti"],
			['await import /* call */ ("jiti");', "jiti"],
			['await import( // arg\n\t"jiti",\n);', "jiti"],
			// Template expressions are code, at any nesting depth.
			['const s = `${await import("jiti")}`;', "jiti"],
			['const s = `raw ${ `${ import("jiti") }` } raw`;', "jiti"],
			// Comments inside a static clause do not hide the specifier.
			['import { x } /* bindings */ from "jiti";', "jiti"],
			['import.meta.url; await import("jiti");', "jiti"],
		];
		for (const [source, expected] of cases) {
			expect(findExcludedImport(source)).toBe(expected);
		}
	});

	test("lexical import scanner skips regular-expression literals without swallowing division", () => {
		const cases: Array<[source: string, expected: string | undefined]> = [
			['const r = /`/; import "jiti";', "jiti"],
			['const r = /[`/]/; import "jiti";', "jiti"],
			['const r = /\\//; import "jiti";', "jiti"],
			['const r = /`/gu; const ready = true; import "jiti";', "jiti"],
			['const quotient = value / import("jiti") / divisor;', "jiti"],
		];
		for (const [source, expected] of cases) {
			expect(findExcludedImport(source)).toBe(expected);
		}
	});

	test("lexical import scanner skips a regex after an unambiguous prefix operator", () => {
		expect(findExcludedImport('const matched = !/`/.test(value); import "jiti";')).toBe("jiti");
	});

	test("lexical import scanner skips a regex while scanning an export default clause", () => {
		expect(findExcludedImport('export default /`/; import "jiti";')).toBe("jiti");
	});

	test("lexical import scanner recognizes a regex after return", () => {
		expect(findExcludedImport("function x(){ return /`/.test(x); } import \"jiti\";")).toBe("jiti");
	});

	test("lexical import scanner recognizes a regex after typeof", () => {
		expect(findExcludedImport("const x = typeof /`/; import \"jiti\";")).toBe("jiti");
	});

	test("lexical import scanner recognizes a regex after case", () => {
		expect(findExcludedImport("switch(x){case /`/: break;} import \"jiti\";")).toBe("jiti");
	});

	test("lexical import scanner recognizes a regex after an arrow", () => {
		expect(findExcludedImport("const f = () => /`/.test(x); import \"jiti\";")).toBe("jiti");
	});

	test("lexical import scanner recognizes a regex as a binary right operand", () => {
		const cases: Array<[source: string, expected: string | undefined]> = [
			['a / /`/.source; import("jiti")', "jiti"],
			['a > /`/.test(x); import("jiti")', "jiti"],
			['a < /`/.test(x); import("jiti")', "jiti"],
			['a + /`/.source; import("jiti")', "jiti"],
			['a - /`/.source; import("jiti")', "jiti"],
			['const v = +/`/.source; import("jiti")', "jiti"],
		];
		for (const [source, expected] of cases) {
			expect(findExcludedImport(source)).toBe(expected);
		}
	});

	test("lexical import scanner keeps increment and decrement as division", () => {
		// `++`/`--` are single tokens, so the operand before `/` is a value and
		// the slash divides; only a lone `+`/`-` may be followed by a regex.
		const cases: Array<[source: string, expected: string | undefined]> = [
			['const q = x++ / import("jiti") / 2;', "jiti"],
			['const q = x-- / import("jiti") / 2;', "jiti"],
			["const q = x++ / 2;", undefined],
		];
		for (const [source, expected] of cases) {
			expect(findExcludedImport(source)).toBe(expected);
		}
	});

	test("lexical import scanner keeps keyword-shaped identifiers and members as division", () => {
		const cases: Array<[source: string, expected: string | undefined]> = [
			['const myreturn = 4; myreturn / import("jiti") / 2;', "jiti"],
			['const preturn = 4; preturn / import("jiti") / 2;', "jiti"],
			["obj.default / 2;", undefined],
			['obj.default / import("jiti") / 2;', "jiti"],
			['obj.case / import("jiti") / 2;', "jiti"],
		];
		for (const [source, expected] of cases) {
			expect(findExcludedImport(source)).toBe(expected);
		}
	});

	test("parseStreamingJson recovery is bounded to a constant tail", () => {
		// Recovery boundary inside the 512-char tail: recovered in full.
		const longText = "x".repeat(600);
		expect(parseStreamingJson(`{"a":1,"b":"${longText}`)).toEqual({ a: 1, b: longText });
		// Recovery would need a boundary older than the tail: bounded give-up.
		expect(parseStreamingJson(`{"a":1,"b":${"!".repeat(600)}`)).toEqual({});
		// A mismatched closer ends recovery at the last sound prefix.
		expect(parseStreamingJson('{"a":1}]')).toEqual({ a: 1 });
		// A trailing unescaped backslash is dropped before closing the string.
		expect(parseStreamingJson('{"a":"x\\')).toEqual({ a: "x" });
		// A large valid prefix with a hostile tail must recover without reparsing
		// hundreds of near-identical prefixes. The strict parse and one recovery
		// parse are observable; elapsed time is not.
		const parseSpy = vi.spyOn(JSON, "parse");
		const payload = "x".repeat(100_000);
		try {
			expect(parseStreamingJson(`{"payload":"${payload}","broken":${"!".repeat(500)}`)).toEqual({
				payload,
			});
			expect(parseSpy).toHaveBeenCalledTimes(2);
		} finally {
			parseSpy.mockRestore();
		}
		// No full-input backwards retries: 100k of garbage returns cheaply.
		expect(parseStreamingJson("z".repeat(100_000))).toEqual({});
	});
});

// ---------------------------------------------------------------------------
// Cancellation classification: signal/AbortError only, never message text
// ---------------------------------------------------------------------------

describe("lean: cancellation classification", () => {
	test("message text never classifies; only the signal or a structured AbortError does", async () => {
		const directory = await mkdtemp(join(PACKAGE_DIR, ".test-lean-cancel-classify-"));
		const entry = join(directory, "cancel-classify.mjs");
		await writeFile(
			entry,
			`export default {
	name: "cancel-classify",
	tools: [
		{
			name: "says-cancelled",
			description: "Fails with a message that merely mentions cancellation",
			execute: () => {
				throw new Error("operation cancelled by upstream; please abort retries");
			},
		},
		{
			name: "aborts-structurally",
			description: "Throws a structured AbortError without an aborted signal",
			execute: () => {
				const error = new Error("interrupted");
				error.name = "AbortError";
				throw error;
			},
		},
	],
	providers: [
		{
			name: "says-cancelled-provider",
			streamSimple: async function* () {
				throw new Error("stream cancelled midway");
			},
		},
		{
			name: "aborts-structurally-provider",
			streamSimple: async function* () {
				throw new DOMException("interrupted", "AbortError");
			},
		},
	],
};
`,
		);
		const link = new LeanLink({ cwd: directory, extensionPaths: [] });
		try {
			await link.hello(1);
			link.request(2, "extensions.load", { extensionPaths: [entry], cwd: directory });
			expect(payload(await link.response(2, "extensions.load"))["errors"]).toEqual([]);

			link.request(3, "tool.execute", { name: "says-cancelled", toolCallId: "cc-1", args: {}, prepared: true });
			const toolPlain = payload(await link.error(3, "tool.execute"));
			expect(toolPlain["code"]).toBe("extension_error");
			expect(toolPlain["message"]).toBe("operation cancelled by upstream; please abort retries");

			link.request(4, "tool.execute", { name: "aborts-structurally", toolCallId: "cc-2", args: {}, prepared: true });
			const toolAbort = payload(await link.error(4, "tool.execute"));
			expect(toolAbort["code"]).toBe("cancelled");
			expect(toolAbort["message"]).toBe("extension tool cancelled");
			expect(toolAbort["retryable"]).toBe(false);

			link.request(5, "provider.stream", { providerId: "says-cancelled-provider", model: {}, context: {}, options: {} });
			const providerPlain = payload(await link.error(5, "provider.stream"));
			expect(providerPlain["code"]).toBe("extension_error");
			expect(providerPlain["message"]).toBe("stream cancelled midway");

			link.request(6, "provider.stream", { providerId: "aborts-structurally-provider", model: {}, context: {}, options: {} });
			const providerAbort = payload(await link.error(6, "provider.stream"));
			expect(providerAbort["code"]).toBe("cancelled");
			expect(providerAbort["message"]).toBe("provider stream cancelled");
			expect(providerAbort["retryable"]).toBe(false);
		} finally {
			await link.finish();
			await rm(directory, { recursive: true, force: true });
		}
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
		expect(basenames.sort()).toEqual([
			"assistant-delta.ts",
			"lean-api.ts",
			"lean-runner.ts",
			"protocol.ts",
		]);
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
		try {
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

			// Each resolve-log line is "<specifier>\t<importer>", mirroring the
			// production onResolve hook (args.path then args.importer). The
			// production FORBIDDEN gate tests only the specifier column, so these
			// assertions key off it too — a raw substring search over the whole
			// log false-matches node_modules importer paths: the old
			// /(?:node:)?module/ regex matched the "module" inside "node_modules".
			const log = readFileSync(resolveLog, "utf8");
			const specifiers = log
				.split("\n")
				.filter((line) => line.trim() !== "")
				.map((line) => {
					const tab = line.indexOf("\t");
					return tab === -1 ? line : line.slice(0, tab);
				});

			// Positive evidence: the lean entry resolved.
			expect(specifiers).toContain("./lean-runner.ts");
			// Compat-graph specifiers never resolved — matched on the specifier
			// column, never the importer.
			expect(specifiers.some((s) => s.includes("host.ts"))).toBe(false);
			expect(specifiers.some((s) => s.includes("virtual-modules"))).toBe(false);
			expect(specifiers.some((s) => s.includes("pi-coding-agent"))).toBe(false);
			// The module specifier is matched exactly: only the bare specifier
			// "module" or "node:module" is forbidden, never a node_modules path.
			expect(specifiers).not.toContain("module");
			expect(specifiers).not.toContain("node:module");
		} finally {
			// Never orphan the graph-proof subprocess: a failed expect or a
			// withTimeout rejection on a wedged child leaves it running with an
			// open stdin that never exits on its own.
			child.stdin.destroy();
			if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
			await rm(scratch, { recursive: true, force: true });
		}
	}, 30_000);
});
