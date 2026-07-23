/**
 * Lean extension runner (Mode 2).
 *
 * Speaks the existing JSONL protocol over stdin/stdout using the shared
 * ProtocolClient, but owns ZERO upstream runtime graph: no host.ts, no
 * builtins, no virtual-modules, no jiti, no `@earendil-works/pi-coding-agent`.
 * Entries are strict manifest-selected, prebundled `.mjs` files loaded with
 * a plain dynamic `import()` and validated against the declarative lean API
 * (`lean-api.ts`). Unknown/unsupported module surfaces become per-extension
 * load errors; the runner itself stays up.
 *
 * Wire parity with Mode 1 (`host.ts`): identical method names, payload
 * shapes, registry snapshot (RegistrySnapshotWire-compatible), error codes,
 * and hook response shaping. The only intentional deviation is the hello
 * handshake: Mode 2 validates `protocolVersion` ONLY and ignores
 * `compatibilityVersion`.
 */

import { readFile } from "node:fs/promises";
import { isAbsolute, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import {
	COMPATIBILITY_VERSION,
	type ByteReadable,
	type ByteWritable,
	type Frame,
	type FrameHandler,
	type Method,
	PROTOCOL_VERSION,
	ProtocolClient,
} from "./protocol.ts";
import {
	LEAN_EVENT_TYPES,
	type LeanCommand,
	type LeanContext,
	type LeanExtension,
	type LeanFlag,
	type LeanProvider,
	type LeanShortcut,
	type LeanTool,
	parseLeanExtension,
} from "./lean-api.ts";

/** Host lifecycle state. */
const RunnerState = {
	WAITING_HELLO: "WAITING_HELLO",
	LOADING: "LOADING",
	READY: "READY",
	DISPOSED: "DISPOSED",
} as const;
type RunnerState = (typeof RunnerState)[keyof typeof RunnerState];

/** Pending load options captured before the hello handshake completes. */
interface LoadOptions {
	cwd: string;
	extensionPaths: string[];
}

interface RegisteredTool {
	tool: LeanTool;
	extensionPath: string;
}
interface RegisteredCommand {
	command: LeanCommand;
	extensionPath: string;
}
interface RegisteredFlag {
	flag: LeanFlag;
	extensionPath: string;
}
interface RegisteredShortcut {
	key: string;
	shortcut: LeanShortcut;
	extensionPath: string;
}
interface RegisteredProvider {
	provider: LeanProvider;
	extensionPath: string;
}
interface RegisteredHook {
	handler: (event: never, ctx: LeanContext) => unknown;
	extensionPath: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Canonicalize JSON-like values so object key order does not affect equality.
 * Arrays keep element order; plain objects get sorted keys recursively.
 */
function canonicalizeJson(value: unknown): unknown {
	if (Array.isArray(value)) {
		return value.map(canonicalizeJson);
	}
	if (isRecord(value)) {
		const sorted: Record<string, unknown> = {};
		for (const key of Object.keys(value).sort()) {
			sorted[key] = canonicalizeJson(value[key]);
		}
		return sorted;
	}
	return value;
}

/** Order-insensitive JSON structural equality (key reorder is a no-op). */
function jsonEqual(a: unknown, b: unknown): boolean {
	return JSON.stringify(canonicalizeJson(a)) === JSON.stringify(canonicalizeJson(b));
}

/**
 * Import specifiers a lean entry must never reference. Lean entries are
 * prebundled, so ANY upstream-compat specifier means the entry was built
 * for the wrong mode. `@earendil-works/pi-tui-protocol` stays legal: it is
 * the shared wire package, not the upstream runtime graph.
 */
const EXCLUDED_SPECIFIER = /^(?:@earendil-works\/(?:pi-coding-agent|pi-agent-core|pi-ai|pi-tui(?!-protocol))|@mariozechner\/|jiti(?:\/|$)|typebox(?:\/|$)|.*\/(?:host|virtual-modules)\.ts$)/;

/**
 * ONE boundary-aware scan over every module-specifier form: static
 * `import … from` / `export … from` (including minified `import{x}from"…"`,
 * `export{x}from"…"`, `export*from"…"`), dynamic `import("…")`, and
 * side-effect `import "…"`. Identifier boundaries reject keyword-like
 * identifiers (`important`, `exporter`), member calls (`a.import("…")`),
 * and `import.meta`; quotes/parens/backticks cannot bridge clauses across
 * statements.
 */
const IMPORT_SPECIFIER =
	/(?<![\w$.])(?:import|export)(?![\w$.])[^"'()`]*?\bfrom\s*["']([^"']+)["']|(?<![\w$.])import(?![\w$.])\s*\(\s*["']([^"']+)["']\s*\)|(?<![\w$.])import(?![\w$.])\s*["']([^"']+)["']/g;

/**
 * Best-effort detection of excluded imports in a prebundled entry. The
 * compiled-fixture suite proves graph absence independently; this scan turns
 * the detectable cases into precise per-extension load errors.
 */
export function findExcludedImport(source: string): string | undefined {
	for (const match of source.matchAll(IMPORT_SPECIFIER)) {
		const specifier = match[1] ?? match[2] ?? match[3];
		if (specifier !== undefined && EXCLUDED_SPECIFIER.test(specifier)) {
			return specifier;
		}
	}
	return undefined;
}

/**
 * Tolerant parse of possibly-incomplete streamed JSON (toolcall argument
 * fragments). Mirrors upstream `parseStreamingJson`: strict parse first,
 * then progressively trim to the longest prefix that closes into valid
 * JSON; `{}` when nothing parses.
 */
export function parseStreamingJson(text: string | undefined): Record<string, unknown> {
	if (text === undefined || text.trim() === "") {
		return {};
	}
	try {
		const strict: unknown = JSON.parse(text);
		return isRecord(strict) ? strict : {};
	} catch {
		// Fall through to tolerant trimming.
	}
	for (let end = text.length; end > 0; end--) {
		const closed = closePartialJson(text.slice(0, end));
		if (closed === undefined) continue;
		try {
			const parsed: unknown = JSON.parse(closed);
			return isRecord(parsed) ? parsed : {};
		} catch {
			// Keep trimming.
		}
	}
	return {};
}

/**
 * Close a JSON prefix: balance an unterminated string and any open
 * arrays/objects. Returns undefined when the prefix is structurally
 * unrecoverable (e.g. a stray closing bracket).
 */
function closePartialJson(prefix: string): string | undefined {
	let inString = false;
	let escaped = false;
	const stack: string[] = [];
	for (const char of prefix) {
		if (inString) {
			if (escaped) {
				escaped = false;
			} else if (char === "\\") {
				escaped = true;
			} else if (char === '"') {
				inString = false;
			}
			continue;
		}
		if (char === '"') {
			inString = true;
		} else if (char === "{" || char === "[") {
			stack.push(char);
		} else if (char === "}" || char === "]") {
			const open = stack.pop();
			if (open === undefined) return undefined;
			if ((open === "{" && char !== "}") || (open === "[" && char !== "]")) {
				return undefined;
			}
		}
	}
	let closed = prefix;
	if (inString) {
		// A trailing escape would escape the closing quote; drop it first.
		if (escaped) closed = closed.slice(0, -1);
		closed += '"';
	}
	for (let index = stack.length - 1; index >= 0; index--) {
		closed += stack[index] === "{" ? "}" : "]";
	}
	return closed;
}

/**
 * Lean Mode-2 endpoint process. Owns the declarative registry and bridges
 * it to Rust over a single JSONL byte transport, mirroring the Mode-1
 * host's four-state machine (hello → loading → ready → disposed).
 */
export class LeanRunner {
	private readonly client: ProtocolClient;
	private state: RunnerState = RunnerState.WAITING_HELLO;
	private loadOptions: LoadOptions | undefined;
	private cwd: string = process.cwd();
	/** Frames buffered while extensions are loading. */
	private readonly pendingFrames: Frame[] = [];

	private readonly tools = new Map<string, RegisteredTool>();
	private readonly commands = new Map<string, RegisteredCommand>();
	private readonly flags = new Map<string, RegisteredFlag>();
	private readonly flagValues = new Map<string, boolean | string>();
	private readonly shortcuts: RegisteredShortcut[] = [];
	private readonly providers = new Map<string, RegisteredProvider>();
	private readonly hooks = new Map<string, RegisteredHook[]>();
	private loadedCount = 0;

	/** In-flight tool.execute AbortControllers keyed by request id. */
	private readonly inFlightTools = new Map<number, AbortController>();
	/** In-flight provider.stream AbortControllers keyed by request id. */
	private readonly inFlightProviders = new Map<number, AbortController>();
	/** System prompt mirrored from `session.update` control events. */
	private systemPrompt = "";
	/** Active assistant snapshot reconstructed from compact Rust updates. */
	private activeAssistant: Record<string, unknown> | undefined;
	/** Raw streamed tool-argument fragments keyed by assistant content index. */
	private readonly activeToolArguments = new Map<number, string>();

	constructor(stdin: ByteReadable, stdout: ByteWritable) {
		const onFrame: FrameHandler = (frame) => this.onInbound(frame);
		this.client = new ProtocolClient(stdout, { onFrame });
		this.client.start(stdin);
	}

	/**
	 * Configure load options and run until EOF/dispose. The hello handshake
	 * drives the rest: once the protocol version matches, CLI-selected
	 * extensions load and the runner enters the serving loop.
	 */
	run(options: LoadOptions): Promise<void> {
		this.loadOptions = options;
		this.cwd = options.cwd;
		return this.client.join();
	}

	get isDisposed(): boolean {
		return this.state === RunnerState.DISPOSED;
	}

	get extensionCount(): number {
		return this.loadedCount;
	}

	// -----------------------------------------------------------------------
	// Inbound frame state machine
	// -----------------------------------------------------------------------

	private onInbound(frame: Frame): void {
		switch (this.state) {
			case RunnerState.WAITING_HELLO:
				this.handleHelloFrame(frame);
				return;
			case RunnerState.LOADING:
				this.pendingFrames.push(frame);
				return;
			case RunnerState.READY:
				if (frame.kind === "req") {
					this.handleRequest(frame).catch((err) => {
						this.respondError(frame, err instanceof Error ? err.message : String(err));
					});
				} else if (frame.kind === "event") {
					this.handleControlEvent(frame);
				}
				return;
			case RunnerState.DISPOSED:
				return;
		}
	}

	/**
	 * Mode-2 handshake: validate `protocolVersion` ONLY. The
	 * `compatibilityVersion` field is ignored entirely — lean endpoints do
	 * not track the upstream coding-agent version.
	 */
	private handleHelloFrame(frame: Frame): void {
		if (frame.method !== "hello") {
			this.terminate(`expected hello as first frame, got: ${frame.method}`);
			return;
		}
		const payload = frame.payload as Record<string, unknown>;
		const remoteProtocol = payload["protocolVersion"];
		if (typeof remoteProtocol !== "number" || remoteProtocol !== PROTOCOL_VERSION) {
			this.terminate(
				`protocol version mismatch: remote=${String(remoteProtocol)} local=${PROTOCOL_VERSION}`,
			);
			return;
		}
		this.client.respond(frame.id, "hello", {
			protocolVersion: PROTOCOL_VERSION,
			compatibilityVersion: COMPATIBILITY_VERSION,
		}).then(
			() => this.startLoading(),
			(err) => this.terminate(err instanceof Error ? err.message : String(err)),
		);
	}

	private async startLoading(): Promise<void> {
		this.state = RunnerState.LOADING;
		const opts = this.loadOptions;
		if (opts === undefined) {
			this.terminate("no load options");
			return;
		}
		const { errors } = await this.loadAll(opts.extensionPaths, opts.cwd);
		for (const error of errors) {
			this.emitExtensionError(error.path, "load", error.error);
		}
		this.state = RunnerState.READY;

		const buffered = [...this.pendingFrames];
		this.pendingFrames.length = 0;
		for (const frame of buffered) {
			this.onInbound(frame);
		}
	}

	// -----------------------------------------------------------------------
	// Extension loading
	// -----------------------------------------------------------------------

	/**
	 * Load strict manifest-selected prebundled `.mjs` entries with a plain
	 * dynamic import. Every failure — wrong extension, excluded import,
	 * import error, unknown/unsupported module surface — is isolated to its
	 * own entry in `errors`; siblings still bind.
	 */
	private async loadAll(
		paths: readonly string[],
		cwd: string,
	): Promise<{ loaded: number; errors: Array<{ path: string; error: string }> }> {
		const errors: Array<{ path: string; error: string }> = [];
		let loaded = 0;
		for (const entryPath of paths) {
			try {
				await this.loadOne(entryPath, cwd);
				loaded++;
			} catch (err) {
				errors.push({
					path: entryPath,
					error: err instanceof Error ? err.message : String(err),
				});
			}
		}
		this.loadedCount += loaded;
		return { loaded, errors };
	}

	private async loadOne(entryPath: string, cwd: string): Promise<void> {
		if (!entryPath.endsWith(".mjs")) {
			throw new Error(
				`lean entries must be prebundled .mjs files (manifest runtime "ts-lean"): ${entryPath}`,
			);
		}
		const absolute = isAbsolute(entryPath) ? entryPath : resolve(cwd, entryPath);
		const source = await readFile(absolute, "utf8");
		const excluded = findExcludedImport(source);
		if (excluded !== undefined) {
			throw new Error(
				`excluded import "${excluded}" in lean entry: the upstream module graph ` +
					"is unavailable in lean mode; prebundle the entry instead",
			);
		}
		// Dynamic import is required: the entry specifier is runtime-selected
		// (manifest-chosen plugin path), so a static import cannot work.
		const module: unknown = await import(pathToFileURL(absolute).href);
		const definition = parseLeanExtension(
			isRecord(module) ? module["default"] : undefined,
		);
		this.register(definition, entryPath);
	}

	/** Bind one validated definition into the registry (first-wins, load order). */
	private register(definition: LeanExtension, extensionPath: string): void {
		for (const tool of definition.tools ?? []) {
			if (!this.tools.has(tool.name)) {
				this.tools.set(tool.name, { tool, extensionPath });
			}
		}
		for (const command of definition.commands ?? []) {
			if (!this.commands.has(command.name)) {
				this.commands.set(command.name, { command, extensionPath });
			}
		}
		for (const flag of definition.flags ?? []) {
			if (!this.flags.has(flag.name)) {
				this.flags.set(flag.name, { flag, extensionPath });
			}
		}
		for (const shortcut of definition.shortcuts ?? []) {
			this.shortcuts.push({ key: shortcut.key, shortcut, extensionPath });
		}
		for (const provider of definition.providers ?? []) {
			if (!this.providers.has(provider.name)) {
				this.providers.set(provider.name, { provider, extensionPath });
			}
		}
		for (const [eventType, handler] of Object.entries(definition.hooks ?? {})) {
			if (handler === undefined) continue;
			const list = this.hooks.get(eventType) ?? [];
			list.push({
				handler: handler as RegisteredHook["handler"],
				extensionPath,
			});
			this.hooks.set(eventType, list);
		}
	}

	private hookContext(extensionPath: string): LeanContext {
		return { cwd: this.cwd, extensionPath };
	}

	// -----------------------------------------------------------------------
	// Registry snapshot (RegistrySnapshotWire-compatible)
	// -----------------------------------------------------------------------

	private buildRegistrySnapshot(): Record<string, unknown> {
		const tools = [...this.tools.values()].map(({ tool }) => {
			const entry: Record<string, unknown> = {
				name: tool.name,
				label: tool.label ?? tool.name,
				description: tool.description,
				parameters: tool.parameters ?? {},
			};
			if (tool.executionMode !== undefined) {
				entry["executionMode"] = tool.executionMode;
			}
			return entry;
		});

		const commands = [...this.commands.values()].map(({ command, extensionPath }) => ({
			name: command.name,
			description: command.description,
			source: extensionPath,
		}));

		const shortcuts = this.shortcuts.map(({ key, shortcut, extensionPath }) => ({
			key,
			description: shortcut.description,
			extensionPath,
		}));

		const flags = [...this.flags.values()].map(({ flag, extensionPath }) => {
			const entry: Record<string, unknown> = {
				name: flag.name,
				description: flag.description,
				type: flag.type,
				extensionPath,
			};
			if (flag.default !== undefined) {
				entry["default"] = flag.default;
			}
			if (this.flagValues.has(flag.name)) {
				entry["value"] = this.flagValues.get(flag.name);
			}
			return entry;
		});

		const providers = [...this.providers.values()].map(({ provider, extensionPath }) => {
			const entry: Record<string, unknown> = {
				name: provider.name,
				streamSimple: typeof provider.streamSimple === "function",
				extensionPath,
			};
			if (provider.baseUrl !== undefined) entry["baseUrl"] = provider.baseUrl;
			if (provider.api !== undefined) entry["api"] = provider.api;
			if (provider.displayName !== undefined) entry["displayName"] = provider.displayName;
			if (provider.apiKey !== undefined) entry["apiKey"] = provider.apiKey;
			if (provider.headers !== undefined) entry["headers"] = provider.headers;
			if (provider.authHeader !== undefined) entry["authHeader"] = provider.authHeader;
			if (provider.models !== undefined) entry["models"] = provider.models;
			return entry;
		});

		const handlers = LEAN_EVENT_TYPES.filter((eventType) => this.hooks.has(eventType));

		return {
			tools,
			commands,
			shortcuts,
			flags,
			renderers: [],
			providers,
			handlers,
			terminalInput: false,
		};
	}

	// -----------------------------------------------------------------------
	// Request dispatch (Rust → lean runner)
	// -----------------------------------------------------------------------

	private async handleRequest(frame: Frame): Promise<void> {
		const { id, method, payload } = frame;
		const p = isRecord(payload) ? payload : {};

		switch (method) {
			case "extensions.load":
				await this.handleExtensionsLoad(id, p);
				return;
			case "command.execute":
				await this.handleCommandExecute(id, p);
				return;
			case "tool.prepare":
				await this.handleToolPrepare(id, p);
				return;
			case "tool.validate":
				await this.handleToolValidate(id, p);
				return;
			case "tool.execute":
				await this.handleToolExecute(id, p);
				return;
			case "tool.renderHtml":
				// Lean tools carry no renderers; mirror Mode 1's no-renderer reply.
				await this.client.respond(id, "tool.renderHtml" as Method, {});
				return;
			case "provider.stream":
				await this.handleProviderStream(id, p);
				return;
			case "flags.set":
				await this.handleFlagsSet(id, p);
				return;
			case "shortcut.execute":
				await this.handleShortcutExecute(id, p);
				return;
			case "message_update_delta":
				await this.handleMessageUpdateDelta(id, p);
				return;
			default:
				if (this.hooks.has(method)) {
					await this.handleLifecycleHook(id, method, p);
					return;
				}
				this.respondError(frame, `unknown method: ${method}`);
		}
	}

	private async handleExtensionsLoad(id: number, p: Record<string, unknown>): Promise<void> {
		this.clearActiveAssistant();
		const paths = p["extensionPaths"] ?? p["paths"];
		const extensionPaths = Array.isArray(paths)
			? paths.filter((path): path is string => typeof path === "string")
			: [];
		const cwd = typeof p["cwd"] === "string" ? p["cwd"] : this.cwd;
		this.cwd = cwd;

		const { loaded, errors } = await this.loadAll(extensionPaths, cwd);
		const snapshot = this.buildRegistrySnapshot();
		await this.client.respond(id, "extensions.load" as Method, {
			...snapshot,
			extensions: loaded,
			errors,
		});
	}

	private async handleCommandExecute(id: number, p: Record<string, unknown>): Promise<void> {
		const commandName = (p["command"] ?? p["name"]) as string;
		const args = (p["args"] as string) ?? "";
		const registered = this.commands.get(commandName);
		if (registered === undefined) {
			await this.client.respondError(id, "command.execute" as Method, {
				code: "not_found",
				message: `Command not found: ${commandName}`,
				retryable: false,
			});
			return;
		}
		try {
			await registered.command.handler(args, this.hookContext(registered.extensionPath));
			await this.client.respond(id, "command.execute" as Method, { ok: true });
		} catch (err) {
			await this.client.respondError(id, "command.execute" as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	private async handleToolPrepare(id: number, p: Record<string, unknown>): Promise<void> {
		const name = String(p["name"] ?? "");
		const args = p["args"];
		const registered = this.tools.get(name);
		if (registered === undefined) {
			await this.client.respondError(id, "tool.prepare" as Method, {
				code: "not_found",
				message: `Tool not found: ${name}`,
				retryable: false,
			});
			return;
		}
		try {
			const prepared = registered.tool.prepare !== undefined
				? await registered.tool.prepare(args, this.hookContext(registered.extensionPath))
				: args;
			await this.client.respond(id, "tool.prepare" as Method, { args: prepared });
		} catch (err) {
			await this.client.respondError(id, "tool.prepare" as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	private async handleToolValidate(id: number, p: Record<string, unknown>): Promise<void> {
		const name = String(p["name"] ?? "");
		const args = p["args"];
		const registered = this.tools.get(name);
		if (registered === undefined) {
			await this.client.respondError(id, "tool.validate" as Method, {
				code: "not_found",
				message: `Tool not found: ${name}`,
				retryable: false,
			});
			return;
		}
		try {
			const validated = registered.tool.validate !== undefined
				? await registered.tool.validate(args, this.hookContext(registered.extensionPath))
				: args;
			await this.client.respond(id, "tool.validate" as Method, { args: validated });
		} catch (err) {
			await this.client.respondError(id, "tool.validate" as Method, {
				code: "invalid_arguments",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	private async handleToolExecute(id: number, p: Record<string, unknown>): Promise<void> {
		const name = String(p["name"] ?? "");
		const toolCallId = String(p["toolCallId"] ?? "");
		const args = p["args"];
		const registered = this.tools.get(name);
		if (registered === undefined) {
			await this.client.respondError(id, "tool.execute" as Method, {
				code: "not_found",
				message: `Tool not found: ${name}`,
				retryable: false,
			});
			return;
		}

		const controller = new AbortController();
		this.inFlightTools.set(id, controller);
		try {
			const prepared = p["prepared"] === true || registered.tool.prepare === undefined
				? args
				: await registered.tool.prepare(args, this.hookContext(registered.extensionPath));
			const result = await registered.tool.execute(prepared, {
				cwd: this.cwd,
				extensionPath: registered.extensionPath,
				toolCallId,
				signal: controller.signal,
				onUpdate: (partial) => {
					this.client.send({
						id,
						kind: "event",
						method: "toolUpdate",
						payload: { toolCallId, toolName: name, partialResult: partial },
					}).catch(() => void 0);
				},
			});
			if (controller.signal.aborted) {
				await this.client.respondError(id, "tool.execute" as Method, {
					code: "cancelled",
					message: "extension tool cancelled",
					retryable: false,
				});
				return;
			}
			await this.client.respond(id, "tool.execute" as Method, result);
		} catch (err) {
			const message = err instanceof Error ? err.message : String(err);
			const cancelled = controller.signal.aborted
				|| message.toLowerCase().includes("abort")
				|| message.toLowerCase().includes("cancel");
			await this.client.respondError(id, "tool.execute" as Method, {
				code: cancelled ? "cancelled" : "extension_error",
				message: cancelled ? "extension tool cancelled" : message,
				retryable: false,
			});
		} finally {
			this.inFlightTools.delete(id);
		}
	}

	private async handleProviderStream(id: number, p: Record<string, unknown>): Promise<void> {
		const providerId = String(p["providerId"] ?? p["name"] ?? "");
		const registered = this.providers.get(providerId);
		if (registered === undefined || typeof registered.provider.streamSimple !== "function") {
			await this.client.respondError(id, "provider.stream" as Method, {
				code: "not_found",
				message: `Provider not found or missing streamSimple: ${providerId}`,
				retryable: false,
			});
			return;
		}

		const controller = new AbortController();
		this.inFlightProviders.set(id, controller);
		const options = {
			...(isRecord(p["options"]) ? p["options"] : {}),
			signal: controller.signal,
		};
		try {
			const stream = registered.provider.streamSimple(p["model"], p["context"], options);
			for await (const event of stream) {
				if (controller.signal.aborted) break;
				await this.client.send({ id, kind: "event", method: "providerEvent", payload: event });
			}
			if (controller.signal.aborted) {
				await this.client.respondError(id, "provider.stream" as Method, {
					code: "cancelled",
					message: "provider stream cancelled",
					retryable: false,
				});
				return;
			}
			await this.client.respond(id, "provider.stream" as Method, {});
		} catch (err) {
			const message = err instanceof Error ? err.message : String(err);
			const cancelled = controller.signal.aborted
				|| message.toLowerCase().includes("abort")
				|| message.toLowerCase().includes("cancel");
			await this.client.respondError(id, "provider.stream" as Method, {
				code: cancelled ? "cancelled" : "extension_error",
				message: cancelled ? "provider stream cancelled" : message,
				retryable: false,
			});
		} finally {
			this.inFlightProviders.delete(id);
		}
	}

	private async handleFlagsSet(id: number, p: Record<string, unknown>): Promise<void> {
		const values = p["values"];
		if (!isRecord(values)) {
			await this.client.respondError(id, "flags.set", {
				code: "invalid_arguments",
				message: "flags.set values must be an object",
				retryable: false,
			});
			return;
		}
		const entries: Array<readonly [string, boolean | string]> = [];
		for (const [name, value] of Object.entries(values)) {
			if (typeof value !== "boolean" && typeof value !== "string") {
				await this.client.respondError(id, "flags.set", {
					code: "invalid_arguments",
					message: `flags.set value for "${name}" must be boolean or string`,
					retryable: false,
				});
				return;
			}
			entries.push([name, value]);
		}
		for (const [name, value] of entries) {
			this.flagValues.set(name, value);
		}
		await this.client.respond(id, "flags.set", { ok: true });
	}

	private async handleShortcutExecute(id: number, p: Record<string, unknown>): Promise<void> {
		const key = p["key"];
		if (typeof key !== "string") {
			await this.client.respond(id, "shortcut.execute", { handled: false });
			return;
		}
		// Product resolution is last-wins across extensions (load order).
		let registered: RegisteredShortcut | undefined;
		for (let index = this.shortcuts.length - 1; index >= 0; index--) {
			const candidate = this.shortcuts[index];
			if (candidate?.key === key) {
				registered = candidate;
				break;
			}
		}
		if (registered === undefined) {
			await this.client.respond(id, "shortcut.execute", { handled: false });
			return;
		}
		await this.client.respond(id, "shortcut.execute", { handled: true });
		Promise.resolve()
			.then(() => registered.shortcut.handler(this.hookContext(registered.extensionPath)))
			.catch((error) => {
				this.emitExtensionError(
					registered.extensionPath,
					"shortcut.execute",
					error instanceof Error ? error.message : String(error),
				);
			});
	}

	// -----------------------------------------------------------------------
	// Lifecycle hooks
	// -----------------------------------------------------------------------

	/**
	 * Invoke every hook registered for `eventType` in load order, isolating
	 * per-hook failures to `extensionError` events (mirroring the upstream
	 * runner). Result threading mirrors Mode 1 per event type. A thunk event
	 * is rebuilt per handler so ordered folds (input, before_agent_start,
	 * tool_result, message_end) hand each handler the running values, exactly
	 * like the upstream specialized emitters.
	 */
	private async runHooks(
		eventType: string,
		event: Record<string, unknown> | (() => Record<string, unknown>),
		onResult: (result: unknown, extensionPath: string) => boolean | void,
	): Promise<void> {
		for (const { handler, extensionPath } of this.hooks.get(eventType) ?? []) {
			try {
				const currentEvent = typeof event === "function" ? event() : event;
				const result = await handler(currentEvent as never, this.hookContext(extensionPath));
				if (onResult(result, extensionPath) === false) return;
			} catch (err) {
				this.emitExtensionError(
					extensionPath,
					eventType,
					err instanceof Error ? err.message : String(err),
				);
			}
		}
	}

	/**
	 * Drive the declared hooks for a lifecycle request. The method name IS
	 * the event type discriminant; response shaping mirrors Mode 1 exactly.
	 */
	private async handleLifecycleHook(
		id: number,
		eventType: string,
		payload: Record<string, unknown>,
	): Promise<void> {
		try {
			if (eventType === "message_start") {
				const message = payload["message"];
				if (isRecord(message) && message["role"] === "assistant") {
					this.seedActiveAssistant(message);
				}
			}
			switch (eventType) {
				case "tool_call": {
					const input = payload["input"];
					if (!isRecord(input)) throw new Error("tool_call.input is required");
					// Snapshot for omission: Rust treats wire.input = Some as
					// arguments_changed. Only echo input when a handler actually
					// mutated JSON content (key reorder alone is not a change).
					const baseline = structuredClone(input);
					let result: unknown;
					await this.runHooks(eventType, { type: eventType, ...payload, input }, (r) => {
						if (r === undefined || r === null) return;
						result = r;
						if (isRecord(r) && r["block"] === true) return false;
					});
					const response: Record<string, unknown> = {
						...(isRecord(result) ? result : {}),
					};
					if (!jsonEqual(input, baseline)) {
						response["input"] = input;
					} else {
						delete response["input"];
					}
					await this.client.respond(id, eventType as Method, response);
					return;
				}
				case "tool_result": {
					// `current` threads running values to later handlers; `response`
					// is omission-shaped for Rust AfterToolCallWire (presence of a
					// field marks that field changed — never echo untouched payload).
					const current: Record<string, unknown> = {
						content: payload["content"],
						details: payload["details"],
						isError: payload["isError"] === true,
					};
					const response: Record<string, unknown> = {};
					await this.runHooks(eventType, () => ({ type: eventType, ...payload, ...current }), (r) => {
						if (!isRecord(r)) return;
						if (r["content"] !== undefined) {
							current["content"] = r["content"];
							response["content"] = r["content"];
						}
						if (r["details"] !== undefined) {
							current["details"] = r["details"];
							response["details"] = r["details"];
						}
						if (r["isError"] !== undefined) {
							current["isError"] = r["isError"];
							response["isError"] = r["isError"];
						}
						// Explicit `terminate` folds exactly like the other fields:
						// omission retains the running value, a later explicit wins.
						if (r["terminate"] !== undefined) {
							current["terminate"] = r["terminate"];
							response["terminate"] = r["terminate"];
						}
					});
					await this.client.respond(id, eventType as Method, response);
					return;
				}
				case "before_agent_start": {
					// Cross-endpoint folds carry the running prompt in the payload;
					// the session.update mirror is only the single-endpoint fallback.
					let systemPrompt =
						typeof payload["systemPrompt"] === "string"
							? payload["systemPrompt"]
							: this.systemPrompt;
					const messages: unknown[] = [];
					let systemPromptModified = false;
					await this.runHooks(
						eventType,
						() => ({
							type: eventType,
							...payload,
							systemPrompt,
							systemPromptOptions: { cwd: this.cwd },
						}),
						(r) => {
							if (!isRecord(r)) return;
							if (r["message"] !== undefined && r["message"] !== null) {
								messages.push(r["message"]);
							}
							if (typeof r["systemPrompt"] === "string") {
								systemPrompt = r["systemPrompt"];
								systemPromptModified = true;
							}
						},
					);
					const result: Record<string, unknown> = {};
					if (messages.length > 0) result["messages"] = messages;
					if (systemPromptModified) result["systemPrompt"] = systemPrompt;
					await this.client.respond(id, eventType as Method, result);
					return;
				}
				case "message_end": {
					this.clearActiveAssistant();
					// Rust sends the raw AgentMessage AS the request payload (no
					// `{ message }` wrapper); the payload itself is the running value.
					let currentMessage: unknown = payload;
					let modified = false;
					await this.runHooks(eventType, () => ({ type: eventType, message: currentMessage }), (r, path) => {
						if (!isRecord(r) || !isRecord(r["message"])) return;
						const replacement = r["message"];
						if (
							isRecord(currentMessage)
							&& replacement["role"] !== currentMessage["role"]
						) {
							this.emitExtensionError(
								path,
								eventType,
								"message_end handlers must return a message with the same role",
							);
							return;
						}
						currentMessage = replacement;
						modified = true;
					});
					await this.client.respond(id, eventType as Method, {
						message: modified ? currentMessage : undefined,
					});
					return;
				}
				case "input": {
					let text = payload["text"];
					let images = payload["images"];
					let handled = false;
					await this.runHooks(eventType, () => ({ type: eventType, ...payload, text, images }), (r) => {
						if (!isRecord(r)) return;
						if (r["action"] === "handled") {
							handled = true;
							return false;
						}
						if (r["action"] === "transform") {
							text = r["text"];
							images = r["images"] ?? images;
						}
					});
					if (handled) {
						await this.client.respond(id, eventType as Method, { action: "handled" });
						return;
					}
					const changed = text !== payload["text"] || images !== payload["images"];
					await this.client.respond(
						id,
						eventType as Method,
						changed ? { action: "transform", text, images } : { action: "continue" },
					);
					return;
				}
				case "resources_discover": {
					const discovered: Record<string, unknown[]> = {
						skillPaths: [],
						promptPaths: [],
						themePaths: [],
					};
					await this.runHooks(eventType, { type: eventType, ...payload }, (r, path) => {
						if (!isRecord(r)) return;
						for (const key of ["skillPaths", "promptPaths", "themePaths"] as const) {
							const list = r[key];
							if (!Array.isArray(list)) continue;
							for (const entry of list) {
								if (typeof entry === "string") {
									discovered[key]?.push({ path: entry, extensionPath: path });
								}
							}
						}
					});
					await this.client.respond(id, eventType as Method, discovered);
					return;
				}
				case "session_before_switch":
				case "session_before_fork":
				case "session_before_compact":
				case "session_before_tree": {
					let result: unknown;
					await this.runHooks(eventType, { type: eventType, ...payload }, (r) => {
						if (r === undefined || r === null) return;
						result = r;
						if (isRecord(r) && r["cancel"] === true) return false;
					});
					await this.client.respond(id, eventType as Method, result ?? { ok: true });
					return;
				}
				default: {
					if (eventType === "agent_end" || eventType === "session_shutdown") {
						this.clearActiveAssistant();
					}
					await this.runHooks(eventType, { type: eventType, ...payload }, () => void 0);
					await this.client.respond(id, eventType as Method, { ok: true });
					return;
				}
			}
		} catch (err) {
			await this.client.respondError(id, eventType as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	// -----------------------------------------------------------------------
	// message_update_delta: compact stream → full message_update hook
	// -----------------------------------------------------------------------

	private async handleMessageUpdateDelta(
		id: number,
		payload: Record<string, unknown>,
	): Promise<void> {
		if (!this.hooks.has("message_update")) {
			await this.client.respond(id, "message_update_delta" as Method, { ok: true });
			return;
		}
		const event = payload["event"];
		if (!isRecord(event) || typeof event["type"] !== "string") {
			await this.client.respondError(id, "message_update_delta" as Method, {
				code: "invalid_request",
				message: "message_update_delta.event is required",
				retryable: false,
			});
			return;
		}
		try {
			const type = event["type"];
			const final = event["final"];
			if ((type === "done" || type === "error") && isRecord(final)) {
				const message = structuredClone(final);
				const assistantMessageEvent = type === "done"
					? { type, reason: event["reason"], message }
					: { type, reason: event["reason"], error: message };
				this.clearActiveAssistant();
				let result: unknown;
				await this.runHooks(
					"message_update",
					{ type: "message_update", message, assistantMessageEvent },
					(r) => {
						// CancelWire short-circuit — same fold as session_before_*;
						// any other return keeps the `{ ok: true }` response.
						if (!isRecord(r) || r["cancel"] !== true) return;
						result = r;
						return false;
					},
				);
				await this.client.respond(
					id,
					"message_update_delta" as Method,
					result ?? { ok: true },
				);
				return;
			}

			this.applyAssistantDelta(event);
			if (this.activeAssistant === undefined) {
				throw new Error("message update arrived before assistant start");
			}
			const message = structuredClone(this.activeAssistant);
			const assistantMessageEvent = this.expandAssistantEvent(event, message);
			let result: unknown;
			await this.runHooks(
				"message_update",
				{ type: "message_update", message, assistantMessageEvent },
				(r) => {
					// CancelWire short-circuit — same fold as session_before_*;
					// any other return keeps the `{ ok: true }` response.
					if (!isRecord(r) || r["cancel"] !== true) return;
					result = r;
					return false;
				},
			);
			await this.client.respond(
				id,
				"message_update_delta" as Method,
				result ?? { ok: true },
			);
		} catch (error) {
			await this.client.respondError(id, "message_update_delta" as Method, {
				code: "extension_error",
				message: error instanceof Error ? error.message : String(error),
				retryable: false,
			});
		}
	}

	private seedActiveAssistant(message: Record<string, unknown>): void {
		this.activeAssistant = structuredClone(message);
		this.activeToolArguments.clear();
	}

	private clearActiveAssistant(): void {
		this.activeAssistant = undefined;
		this.activeToolArguments.clear();
	}

	private applyAssistantDelta(event: Record<string, unknown>): void {
		const meta = isRecord(event["meta"]) ? event["meta"] : {};
		if (this.activeAssistant === undefined) {
			if (event["type"] !== "start") {
				throw new Error("message update arrived before assistant start");
			}
			this.activeAssistant = { ...meta, content: [] };
		} else if (event["type"] === "start") {
			this.activeAssistant = { ...meta, content: [] };
			this.activeToolArguments.clear();
		} else {
			const content = this.activeAssistant["content"];
			this.activeAssistant = { ...this.activeAssistant, ...meta, content };
		}
		const content = this.activeAssistant["content"];
		if (!Array.isArray(content)) {
			throw new Error("active assistant content is not an array");
		}
		const index = event["contentIndex"];
		if (typeof index !== "number") return;
		const type = event["type"];
		if (
			(type === "text_start" || type === "thinking_start" || type === "toolcall_start"
				|| type === "text_end" || type === "thinking_end" || type === "toolcall_end")
			&& isRecord(event["block"])
		) {
			content[index] = structuredClone(event["block"]);
			if (type === "toolcall_start") this.activeToolArguments.set(index, "");
			if (type === "toolcall_end") this.activeToolArguments.delete(index);
			return;
		}
		const delta = event["delta"];
		const block = content[index];
		if (typeof delta !== "string" || !isRecord(block)) return;
		if (type === "text_delta") {
			block["text"] = `${typeof block["text"] === "string" ? block["text"] : ""}${delta}`;
		} else if (type === "thinking_delta") {
			block["thinking"] = `${typeof block["thinking"] === "string" ? block["thinking"] : ""}${delta}`;
		} else if (type === "toolcall_delta") {
			const fragments = `${this.activeToolArguments.get(index) ?? ""}${delta}`;
			this.activeToolArguments.set(index, fragments);
			block["arguments"] = parseStreamingJson(fragments);
		}
	}

	private expandAssistantEvent(
		event: Record<string, unknown>,
		partial: Record<string, unknown>,
	): Record<string, unknown> {
		const type = event["type"] as string;
		const expanded: Record<string, unknown> = { type, partial };
		const index = event["contentIndex"];
		if (typeof index === "number") expanded["contentIndex"] = index;
		if (typeof event["delta"] === "string") expanded["delta"] = event["delta"];
		const content = partial["content"];
		const block = Array.isArray(content) && typeof index === "number" ? content[index] : undefined;
		if (type === "text_end" && isRecord(block)) expanded["content"] = block["text"];
		if (type === "thinking_end" && isRecord(block)) expanded["content"] = block["thinking"];
		if (type === "toolcall_end" && isRecord(block)) expanded["toolCall"] = block;
		return expanded;
	}

	// -----------------------------------------------------------------------
	// Control events, errors, shutdown
	// -----------------------------------------------------------------------

	private handleControlEvent(frame: Frame): void {
		if (frame.method === "session.update") {
			const payload = frame.payload;
			if (isRecord(payload) && typeof payload["systemPrompt"] === "string") {
				this.systemPrompt = payload["systemPrompt"];
			}
			return;
		}
		if (frame.method !== "tool.cancel" && frame.method !== "provider.cancel") {
			return;
		}
		const payload = frame.payload;
		if (!isRecord(payload)) return;
		const requestId = typeof payload["id"] === "number" ? payload["id"] : undefined;
		if (requestId === undefined) return;
		this.inFlightTools.get(requestId)?.abort();
		this.inFlightProviders.get(requestId)?.abort();
	}

	private emitExtensionError(path: string, event: string, message: string): void {
		this.client.send({
			id: 0,
			kind: "event",
			method: "extensionError",
			payload: {
				code: "extension_error",
				message: `[${path}] ${event}: ${message}`,
				retryable: false,
			},
		}).catch(() => void 0);
	}

	private respondError(frame: Frame, message: string): void {
		this.client.respondError(frame.id, frame.method as Method, {
			code: "extension_error",
			message,
			retryable: false,
		}).catch(() => void 0);
	}

	private terminate(reason: string): void {
		this.clearActiveAssistant();
		console.error(`[lean] fatal: ${reason}`);
		this.dispose(reason);
	}

	dispose(reason = "lean runner disposed"): void {
		if (this.state === RunnerState.DISPOSED) return;
		this.state = RunnerState.DISPOSED;
		for (const controller of this.inFlightTools.values()) controller.abort();
		this.inFlightTools.clear();
		for (const controller of this.inFlightProviders.values()) controller.abort();
		this.inFlightProviders.clear();
		this.client.dispose(reason);
	}
}
