/**
 * Extension host bridge: drives the REAL reference `ExtensionRunner` over the
 * structured JSONL protocol. The host is the trusted side — it receives Rust
 * requests, runs extension code, and emits validated structured events back.
 *
 * Three interaction paths (per plan):
 *  1. Native paint never awaits host work.
 *  2. Keystrokes bypass IPC unless an `onTerminalInput` handler or focused
 *     plugin component is registered (4 ms sequential actor).
 *  3. Submit-time transforms, dialogs, and mutable hooks use ID-correlated
 *     control RPCs (30 s timeout, caller-linked cancellation).
 */

import type { ByteReadable, ByteWritable } from "@earendil-works/pi-tui-protocol";
import {
	ProtocolClient,
	type Frame,
	type FrameHandler,
	type Method,
	PROTOCOL_VERSION,
} from "@earendil-works/pi-tui-protocol";

import { COMPATIBILITY_VERSION } from "./version.ts";
import { parseAnsiLines } from "./sanitize.ts";
import type { StyledRun, UiSlot, SlotPlacement, OverlayOptions } from "./protocol.ts";
import { createExtensionJiti } from "./virtual-modules.ts";

import {
	ExtensionRunner,
	loadExtensionFromFactory,
	createExtensionRuntime,
} from "@earendil-works/pi-coding-agent";
import type {
	Extension,
	ExtensionActions,
	ExtensionContextActions,
	ExtensionFactory,
	ExtensionRuntime,
	ExtensionUIContext,
	ProviderConfig,
	ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { EventEmitter } from "node:events";

/** Minimal event bus for extension-to-extension communication. */
export function createEventBus() {
	const emitter = new EventEmitter();
	return {
		emit: (channel: string, data: unknown) => void emitter.emit(channel, data),
		on: (channel: string, handler: (data: unknown) => void) => {
			emitter.on(channel, handler);
			return () => emitter.off(channel, handler);
		},
		clear: () => emitter.removeAllListeners(),
	};
}

/** Lifecycle event type discriminants (mirrors Rust ALL_EVENT_TYPES). */
export const ALL_EVENT_TYPES = [
	"project_trust",
	"resources_discover",
	"session_start",
	"session_info_changed",
	"session_before_switch",
	"session_before_fork",
	"session_before_compact",
	"session_compact",
	"session_shutdown",
	"session_before_tree",
	"session_tree",
	"context",
	"before_provider_request",
	"before_provider_headers",
	"after_provider_response",
	"before_agent_start",
	"agent_start",
	"agent_end",
	"agent_settled",
	"turn_start",
	"turn_end",
	"message_start",
	"message_update",
	"message_end",
	"tool_execution_start",
	"tool_execution_update",
	"tool_execution_end",
	"model_select",
	"thinking_level_select",
	"tool_call",
	"tool_result",
	"user_bash",
	"input",
] as const;

/** Hook timeout: mutable lifecycle hooks must respond within 30 s. */
export const EXTENSION_HOOK_TIMEOUT_MS = 30_000;
/** Input timeout: terminal-input consume/rewrite must respond within 4 ms. */
export const EXTENSION_INPUT_TIMEOUT_MS = 4;
/** Bound on the sequential terminal-input actor queue (plan capacity-64). */
export const EXTENSION_INPUT_QUEUE_CAPACITY = 64;

/** Result returned by a terminal-input consume/rewrite handler. */
export type TerminalInputHandlerResult =
	| { consume?: boolean; data?: string }
	| undefined
	| void;

/** Terminal-input handler registered via `ui.onTerminalInput`. */
export type TerminalInputHandler = (
	data: string,
) => TerminalInputHandlerResult | Promise<TerminalInputHandlerResult>;

interface RegisteredTerminalHandler {
	id: number;
	handler: TerminalInputHandler;
	disabled: boolean;
}

/** Host lifecycle state. */
const HostState = {
	WAITING_HELLO: "WAITING_HELLO",
	LOADING: "LOADING",
	READY: "READY",
	DISPOSED: "DISPOSED",
} as const;
type HostState = (typeof HostState)[keyof typeof HostState];

/** State tracked for a keyed UI slot (widget/header/footer/editor/overlay). */
interface SlotEntry {
	generation: number;
	component: { render(width: number): string[]; dispose?(): void } | null;
	placement: SlotPlacement;
	focusable: boolean;
	overlayOptions: OverlayOptions | undefined;
}

/** Pending load options captured during the hello handshake. */
interface LoadOptions {
	cwd: string;
	extensionPaths: string[];
	factories: ExtensionFactory[];
}

interface ExtensionsLoadRequest {
	extensionPaths: string[];
	cwd: string;
	projectTrusted: boolean;
}

function parseExtensionsLoadRequest(
	payload: Record<string, unknown>,
	fallbackCwd: string,
): ExtensionsLoadRequest {
	const paths = payload["extensionPaths"] ?? payload["paths"];
	return {
		extensionPaths: Array.isArray(paths)
			? paths.filter((path): path is string => typeof path === "string")
			: [],
		cwd: typeof payload["cwd"] === "string" ? payload["cwd"] : fallbackCwd,
		projectTrusted: payload["projectTrusted"] === true,
	};
}

/**
 * Extension host process. Owns the ExtensionRunner and bridges it to Rust over
 * a single JSONL byte transport. One stdout writer; all writes are ordered
 * through the ProtocolClient's internal queue.
 */
export class ExtensionHost {
	private readonly client: ProtocolClient;
	private readonly slots = new Map<string, SlotEntry>();
	private nextGeneration = 1;
	private state: HostState = HostState.WAITING_HELLO;
	private runner: ExtensionRunner | undefined;
	private runtime: ExtensionRuntime | undefined;
	private extensions: Extension[] = [];
	/** Captured custom providers (first registration wins). */
	private readonly providers = new Map<string, ProviderConfig>();
	/** In-flight tool.execute AbortControllers keyed by request id. */
	private readonly inFlightTools = new Map<number, AbortController>();
	/** In-flight provider.stream AbortControllers keyed by request id. */
	private readonly inFlightProviders = new Map<number, AbortController>();
	private loadOptions: LoadOptions | undefined;
	private projectTrusted = false;
	/** Frames buffered while extensions are loading. */
	private readonly pendingFrames: Frame[] = [];
	/** Registered terminal-input handlers (sequential actor). */
	private readonly terminalHandlers: RegisteredTerminalHandler[] = [];
	private nextTerminalHandlerId = 1;
	/** Capacity-64 sequential queue of terminal-input jobs. */
	private readonly terminalInputQueue: Array<() => Promise<void>> = [];
	private terminalInputDraining = false;

	constructor(stdin: ByteReadable, stdout: ByteWritable) {
		const onFrame: FrameHandler = (frame) => this.onInbound(frame);
		this.client = new ProtocolClient(stdout, { onFrame });
		this.client.start(stdin);
	}

	/**
	 * Configure load options and run until EOF/dispose. The hello handshake
	 * drives the rest: once versions match, extensions load and the host
	 * enters the serving loop.
	 */
	run(options: {
		cwd: string;
		extensionPaths: string[];
		factories?: ExtensionFactory[];
	}): Promise<void> {
		this.loadOptions = {
			cwd: options.cwd,
			extensionPaths: options.extensionPaths,
			factories: options.factories ?? [],
		};
		return this.client.join();
	}

	// -----------------------------------------------------------------------
	// Inbound frame state machine
	// -----------------------------------------------------------------------

	/** Single entry point for every inbound frame from Rust. */
	private onInbound(frame: Frame): void {
		switch (this.state) {
			case HostState.WAITING_HELLO:
				this.handleHelloFrame(frame);
				return;
			case HostState.LOADING:
				this.pendingFrames.push(frame);
				return;
			case HostState.READY:
				if (frame.kind === "req") {
					this.handleRequest(frame).catch((err) => {
						this.respondError(frame, err instanceof Error ? err.message : String(err));
					});
				} else if (frame.kind === "event") {
					this.handleControlEvent(frame);
				}
				return;
			case HostState.DISPOSED:
				return;
		}
	}

	/** Validate the hello handshake; terminate on version mismatch. */
	private handleHelloFrame(frame: Frame): void {
		if (frame.method !== "hello") {
			this.terminate(`expected hello as first frame, got: ${frame.method}`);
			return;
		}
		const payload = frame.payload as Record<string, unknown>;
		const remoteProtocol = payload["protocolVersion"];
		const remoteCompat = payload["compatibilityVersion"];
		if (typeof remoteProtocol !== "number" || remoteProtocol !== PROTOCOL_VERSION) {
			this.terminate(
				`protocol version mismatch: remote=${String(remoteProtocol)} local=${PROTOCOL_VERSION}`,
			);
			return;
		}
		if (typeof remoteCompat !== "string" || remoteCompat !== COMPATIBILITY_VERSION) {
			this.terminate(
				`compatibility version mismatch: remote=${String(remoteCompat)} local=${COMPATIBILITY_VERSION}`,
			);
			return;
		}
		// Acknowledge, then start loading.
		this.client.respond(frame.id, "hello", {
			protocolVersion: PROTOCOL_VERSION,
			compatibilityVersion: COMPATIBILITY_VERSION,
		}).then(
			() => this.startLoading(),
			(err) => this.terminate(err instanceof Error ? err.message : String(err)),
		);
	}

	/** Begin extension loading; transitions to READY when complete. */
	private async startLoading(): Promise<void> {
		this.state = HostState.LOADING;
		const opts = this.loadOptions;
		if (opts === undefined) {
			this.terminate("no load options");
			return;
		}
		this.runtime = createExtensionRuntime();
		const eventBus = createEventBus();
		const errors: Array<{ path: string; error: string }> = [];

		for (const factory of opts.factories) {
			try {
				const ext = await loadExtensionFromFactory(
					factory, opts.cwd, eventBus, this.runtime, "<inline>",
				);
				this.extensions.push(ext);
			} catch (err) {
				errors.push({
					path: "<inline>",
					error: err instanceof Error ? err.message : String(err),
				});
			}
		}

		for (const extPath of opts.extensionPaths) {
			try {
				const jiti = createExtensionJiti();
				const module = await jiti.import(extPath, { default: true }) as unknown;
				if (typeof module !== "function") {
					errors.push({
						path: extPath,
						error: `Extension does not export a valid factory function: ${extPath}`,
					});
					continue;
				}
				const ext = await loadExtensionFromFactory(
					module as ExtensionFactory, opts.cwd, eventBus, this.runtime, extPath,
				);
				this.extensions.push(ext);
			} catch (err) {
				errors.push({
					path: extPath,
					error: err instanceof Error ? err.message : String(err),
				});
			}
		}

		// Isolation: failed factories/paths stay in errors; successful ones bind.
		this.rebuildRunner(opts.cwd);
		if (errors.length > 0) {
			for (const e of errors) {
				this.emitExtensionError(e.path, "load", e.error);
			}
		}

		this.state = HostState.READY;

		// Process any frames buffered during loading.
		const buffered = [...this.pendingFrames];
		this.pendingFrames.length = 0;
		for (const f of buffered) {
			this.onInbound(f);
		}
	}

	// -----------------------------------------------------------------------
	// Request dispatch (Rust → host)
	// -----------------------------------------------------------------------

	private async handleRequest(frame: Frame): Promise<void> {
		const { id, method, payload } = frame;
		const p = payload as Record<string, unknown>;

		switch (method) {
			case "measure":
				await this.handleMeasure(id, p);
				return;
			case "render":
				await this.handleRender(id, p);
				return;
			case "terminalInput":
				await this.handleTerminalInput(id, p);
				return;
			case "uiEvent":
				await this.handleUiEvent(id, p);
				return;
			case "extensions.load":
				await this.handleExtensionsLoad(id, p);
				return;
			case "command.execute":
				await this.handleCommandExecute(id, p);
				return;
			case "tool.execute":
				await this.handleToolExecute(id, p);
				return;
			case "tool.prepare":
				await this.handleToolPrepare(id, p);
				return;
			case "tool.validate":
				await this.handleToolValidate(id, p);
				return;
			case "provider.stream":
				await this.handleProviderStream(id, p);
				return;
			default:
				if (this.runner?.hasHandlers(method)) {
					await this.handleLifecycleHook(id, method, p);
					return;
				}
				this.respondError(frame, `unknown method: ${method}`);
		}
	}

	/**
	 * Drive the real ExtensionRunner for a lifecycle hook. The method name IS
	 * the event type discriminant; the result is forwarded to Rust verbatim.
	 */
	private async handleLifecycleHook(
		id: number, eventType: string, payload: Record<string, unknown>,
	): Promise<void> {
		const runner = this.runner;
		if (runner === undefined) {
			await this.client.respondError(id, eventType as Method, {
				code: "extension_error", message: "runner not initialized", retryable: false,
			});
			return;
		}
		try {
			let result: unknown;
			switch (eventType) {
				case "message_end":
					result = await runner.emitMessageEnd({ type: eventType, ...payload });
					await this.client.respond(id, eventType as Method, { message: result ?? undefined });
					return;
				case "input":
					result = await runner.emitInput(
						payload["text"] as string,
						payload["images"],
						payload["source"] as string,
						payload["streamingBehavior"] as string | undefined,
					);
					await this.client.respond(id, eventType as Method, result ?? {});
					return;
				case "resources_discover":
					result = await runner.emitResourcesDiscover(
						payload["cwd"] as string,
						payload["reason"] as string,
					);
					await this.client.respond(id, eventType as Method, result ?? {});
					return;
				case "session_before_compact":
				case "session_compact":
				case "thinking_level_select":
					result = await runner.emit({ type: eventType, ...payload } as Parameters<typeof runner.emit>[0]);
					await this.client.respond(id, eventType as Method, result ?? {});
					return;
				default:
					result = await runner.emit({ type: eventType, ...payload } as Parameters<typeof runner.emit>[0]);
					await this.client.respond(id, eventType as Method, result ?? { ok: true });
					return;
			}
		} catch (err) {
			await this.client.respondError(id, eventType as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	private async handleExtensionsLoad(id: number, p: Record<string, unknown>): Promise<void> {
		const request = parseExtensionsLoadRequest(
			p,
			this.loadOptions?.cwd ?? process.cwd(),
		);
		const { extensionPaths: paths, cwd, projectTrusted } = request;
		const errors: Array<{ path: string; error: string }> = [];
		let loadedCount = 0;

		if (this.runtime === undefined) {
			await this.client.respondError(id, "extensions.load" as Method, {
				code: "extension_error",
				message: "Runtime not initialized",
				retryable: false,
			});
			return;
		}
		this.projectTrusted = projectTrusted;

		const eventBus = createEventBus();

		for (const extPath of paths) {
			try {
				const jiti = createExtensionJiti();
				const module = await jiti.import(extPath, { default: true }) as unknown;
				if (typeof module !== "function") {
					errors.push({
						path: extPath,
						error: `Extension does not export a valid factory function: ${extPath}`,
					});
					continue;
				}
				const ext = await loadExtensionFromFactory(
					module as ExtensionFactory, cwd, eventBus, this.runtime, extPath,
				);
				this.extensions.push(ext);
				loadedCount++;
			} catch (err) {
				errors.push({
					path: extPath,
					error: err instanceof Error ? err.message : String(err),
				});
			}
		}

		// Rebuild so newly loaded tools/providers/handlers bind without killing siblings.
		this.rebuildRunner(cwd);
		const snapshot = this.buildRegistrySnapshot();
		await this.client.respond(id, "extensions.load" as Method, {
			...snapshot,
			extensions: loadedCount,
			errors,
		});
	}

	private async handleCommandExecute(id: number, p: Record<string, unknown>): Promise<void> {
		const commandName = (p["command"] ?? p["name"]) as string;
		const args = (p["args"] as string) ?? "";

		const cmd = this.runner?.getCommand(commandName);
		if (!cmd || !this.runner) {
			await this.client.respondError(id, "command.execute" as Method, {
				code: "not_found", message: `Command not found: ${commandName}`, retryable: false,
			});
			return;
		}

		try {
			await cmd.handler(args, this.runner.createContext());
			await this.client.respond(id, "command.execute" as Method, { ok: true });
		} catch (err) {
			await this.client.respondError(id, "command.execute" as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	private async handleMeasure(id: number, p: Record<string, unknown>): Promise<void> {
		const key = p["key"] as string;
		const width = p["width"] as number;
		const entry = this.slots.get(key);
		const height = entry?.component?.render(width)?.length ?? 0;
		await this.client.respond(id, "measure", { height });
	}

	private async handleRender(id: number, p: Record<string, unknown>): Promise<void> {
		const key = p["key"] as string;
		const width = p["width"] as number;
		const entry = this.slots.get(key);
		const lines = entry?.component?.render(width) ?? [];
		const runs = lines.map((line) => parseAnsiLines(line)[0] ?? []) as StyledRun[][];
		await this.client.respond(id, "render", { runs });
	}

	private async handleTerminalInput(id: number, p: Record<string, unknown>): Promise<void> {
		const data = typeof p["data"] === "string" ? p["data"] : "";
		const result = await this.enqueueTerminalInput(data);
		await this.client.respond(id, "terminalInput", result);
	}

	/**
	 * Register a terminal-input consume/rewrite handler. Returns an unsubscribe
	 * function. Handlers run sequentially under the 4 ms deadline; a timed-out
	 * handler is disabled once and later keys stay local.
	 */
	registerTerminalInputHandler(handler: TerminalInputHandler): () => void {
		const entry: RegisteredTerminalHandler = {
			id: this.nextTerminalHandlerId++,
			handler,
			disabled: false,
		};
		this.terminalHandlers.push(entry);
		return () => {
			const idx = this.terminalHandlers.findIndex((h) => h.id === entry.id);
			if (idx >= 0) this.terminalHandlers.splice(idx, 1);
		};
	}

	/** Number of currently registered (including disabled) handlers. */
	get terminalHandlerCount(): number {
		return this.terminalHandlers.length;
	}

	/** Number of currently active (non-disabled) handlers. */
	get activeTerminalHandlerCount(): number {
		return this.terminalHandlers.filter((h) => !h.disabled).length;
	}

	/**
	 * Enqueue one terminal-input job on the capacity-64 sequential actor.
	 * Queue exhaustion fails open with the original key and emits one
	 * `extension_error` without disabling handlers.
	 */
	private enqueueTerminalInput(
		data: string,
	): Promise<{ consume: boolean; data?: string }> {
		if (this.terminalHandlers.every((h) => h.disabled) || this.terminalHandlers.length === 0) {
			return Promise.resolve({ consume: false, data });
		}
		if (this.terminalInputQueue.length >= EXTENSION_INPUT_QUEUE_CAPACITY) {
			this.emitExtensionError(
				"<terminal-input>",
				"terminalInput",
				`queue exhausted (capacity ${EXTENSION_INPUT_QUEUE_CAPACITY}); original key passed through`,
			);
			return Promise.resolve({ consume: false, data });
		}
		const { promise, resolve } = Promise.withResolvers<{ consume: boolean; data?: string }>();
		this.terminalInputQueue.push(async () => {
			const result = await this.runTerminalInputHandlers(data);
			resolve(result);
		});
		void this.drainTerminalInputQueue();
		return promise;
	}

	private async drainTerminalInputQueue(): Promise<void> {
		if (this.terminalInputDraining) return;
		this.terminalInputDraining = true;
		try {
			while (this.terminalInputQueue.length > 0) {
				const job = this.terminalInputQueue.shift();
				if (job === undefined) break;
				await job();
			}
		} finally {
			this.terminalInputDraining = false;
			if (this.terminalInputQueue.length > 0) {
				void this.drainTerminalInputQueue();
			}
		}
	}

	/**
	 * Run active handlers sequentially. Each handler has its own 4 ms deadline.
	 * On timeout or throw: disable only that handler, emit one extension_error,
	 * and pass the original key through (fail open for that keystroke).
	 */
	private async runTerminalInputHandlers(
		original: string,
	): Promise<{ consume: boolean; data?: string }> {
		let current = original;
		for (const entry of [...this.terminalHandlers]) {
			if (entry.disabled) continue;
			const outcome = await this.invokeTerminalHandler(entry, current);
			if (outcome.kind === "timeout" || outcome.kind === "error") {
				entry.disabled = true;
				this.emitExtensionError(
					"<terminal-input>",
					"terminalInput",
					outcome.kind === "timeout"
						? `handler ${entry.id} exceeded ${EXTENSION_INPUT_TIMEOUT_MS}ms; disabled`
						: `handler ${entry.id} threw: ${outcome.message}`,
				);
				// Fail open: original key passes to native handling; later input
				// stays local because this handler is now disabled.
				return { consume: false, data: original };
			}
			if (outcome.result?.consume) {
				return {
					consume: true,
					data: outcome.result.data ?? current,
				};
			}
			if (outcome.result?.data !== undefined) {
				current = outcome.result.data;
			}
		}
		if (current === original) {
			return { consume: false, data: original };
		}
		return { consume: false, data: current };
	}

	private async invokeTerminalHandler(
		entry: RegisteredTerminalHandler,
		data: string,
	): Promise<
		| { kind: "ok"; result: { consume?: boolean; data?: string } | undefined }
		| { kind: "timeout" }
		| { kind: "error"; message: string }
	> {
		try {
			const raw = entry.handler(data);
			const raced = await Promise.race([
				Promise.resolve(raw).then((value) => ({ kind: "ok" as const, value })),
				new Promise<{ kind: "timeout" }>((resolve) => {
					setTimeout(() => resolve({ kind: "timeout" }), EXTENSION_INPUT_TIMEOUT_MS);
				}),
			]);
			if (raced.kind === "timeout") {
				return { kind: "timeout" };
			}
			const value = raced.value;
			if (value === undefined || value === null || typeof value !== "object") {
				return { kind: "ok", result: undefined };
			}
			const consume =
				"consume" in value && typeof value.consume === "boolean"
					? value.consume
					: undefined;
			const rewritten =
				"data" in value && typeof value.data === "string" ? value.data : undefined;
			return {
				kind: "ok",
				result:
					consume === undefined && rewritten === undefined
						? undefined
						: { consume, data: rewritten },
			};
		} catch (err) {
			return {
				kind: "error",
				message: err instanceof Error ? err.message : String(err),
			};
		}
	}

	private async handleUiEvent(id: number, _p: Record<string, unknown>): Promise<void> {
		await this.client.respond(id, "uiEvent", { ok: true });
	}

	// -----------------------------------------------------------------------
	// Slot management
	// -----------------------------------------------------------------------

	private pushSlot(key: string, entry: SlotEntry, width: number): void {
		if (entry.component === null) return;
		const lines = entry.component.render(width);
		const runs = lines.map((line) => parseAnsiLines(line)[0] ?? []);
		const slot: UiSlot = {
			key,
			generation: entry.generation,
			placement: entry.placement,
			height: lines.length,
			runs,
			focusable: entry.focusable,
		};
		if (entry.overlayOptions !== undefined) slot.overlayOptions = entry.overlayOptions;
		this.client.send({
			id: 0, kind: "event", method: "uiSlot", payload: slot,
		}).catch(() => void 0);
	}

	disposeSlot(key: string): void {
		const entry = this.slots.get(key);
		if (entry === undefined) return;
		entry.component?.dispose?.();
		this.slots.delete(key);
		this.client.send({
			id: 0, kind: "event", method: "disposeSlot",
			payload: { key, generation: entry.generation },
		}).catch(() => void 0);
	}

	// -----------------------------------------------------------------------
	// UI context bridge (extension → Rust dialogs)
	// -----------------------------------------------------------------------

	/**
	 * Build the ExtensionUIContext that forwards dialogs to Rust via correlated
	 * protocol requests.
	 *
	 * The ExtensionUIContext interface is large (~30 methods). Methods that
	 * require a live Rust round-trip (select/confirm/input/editor/notify) send
	 * protocol requests; data-surface methods (setStatus, setWidget, etc.) emit
	 * events or buffer locally. This cast is a documented escape hatch: the
	 * bridge implements the subset extensions use at runtime; unimplemented
	 * methods are inert no-ops.
	 */
	createUIContext(): ExtensionUIContext {
		const self = this;
		const ctx: Record<string, unknown> = {
			select: (title: string, options: string[], opts?: { timeout?: number }) =>
				self.dialog<{ value?: string | null }>("select", {
					title, options, timeoutMs: opts?.timeout,
				}).then((r) => r.value ?? undefined),
			confirm: (title: string, message: string, opts?: { timeout?: number }) =>
				self.dialog<{ confirmed: boolean }>("confirm", {
					title, message, timeoutMs: opts?.timeout,
				}).then((r) => r.confirmed),
			input: (title: string, placeholder?: string, opts?: { timeout?: number }) =>
				self.dialog<{ value?: string | null }>("input", {
					title, placeholder, timeoutMs: opts?.timeout,
				}).then((r) => r.value ?? undefined),
			notify: (message: string, type?: string) => {
				self.client.send({
					id: 0, kind: "event", method: "notify" as Method,
					payload: { message, type: type ?? "info" },
				}).catch(() => void 0);
			},
			onTerminalInput: (handler: TerminalInputHandler) =>
				self.registerTerminalInputHandler(handler),
			setStatus: () => {},
			setWorkingMessage: () => {},
			setWorkingVisible: () => {},
			setWorkingIndicator: () => {},
			setHiddenThinkingLabel: () => {},
			setWidget: (key: string, content: unknown) => {
				if (Array.isArray(content)) {
					const entry: SlotEntry = {
						generation: self.nextGeneration++,
						component: { render: () => content as string[] },
						placement: "aboveEditor",
						focusable: false,
						overlayOptions: undefined,
					};
					self.slots.set(key, entry);
					self.pushSlot(key, entry, 80);
				}
			},
			setFooter: () => {},
			setHeader: () => {},
			setTitle: () => {},
			custom: async (factory: any, options: any) => {
				const { promise, resolve } = Promise.withResolvers<unknown>();
				const key = `overlay.${self.nextGeneration++}`;
				let resolved = false;
				const done = (result: unknown) => {
					if (resolved) return;
					resolved = true;
					self.disposeSlot(key);
					resolve(result);
				};
				Promise.resolve(factory({}, {}, {}, done)).then((component: any) => {
					if (resolved) {
						component?.dispose?.();
						return;
					}
					const entry: SlotEntry = {
						generation: self.nextGeneration++,
						component,
						placement: "overlay",
						focusable: true,
						overlayOptions: (typeof options?.overlayOptions === "function" ? options.overlayOptions() : options?.overlayOptions) ?? {},
					};
					self.slots.set(key, entry);
					self.pushSlot(key, entry, 80);
				}).catch((err) => {
					self.emitExtensionError("<inline>", "custom", String(err));
					done(undefined);
				});
				return promise;
			},
			editor: (title: string, prefill?: string) =>
				self.dialog<{ value?: string | null }>("editor", { title, prefill })
					.then((r) => r.value ?? undefined),
			pasteToEditor: () => {},
			setEditorText: () => {},
			getEditorText: () => "",
			addAutocompleteProvider: () => {},
			setEditorComponent: () => {},
			getEditorComponent: () => undefined,
			theme: {},
			getAllThemes: () => [],
			getTheme: () => undefined,
			setTheme: () => ({ success: false }),
			getToolsExpanded: () => false,
			setToolsExpanded: () => {},
		};
		// Documented escape hatch: the bridge implements the subset extensions
		// use at runtime. The full ExtensionUIContext carries TUI/Theme types
		// that are inert in the headless host.
		return ctx as unknown as ExtensionUIContext;
	}

	private async dialog<T>(method: Method, payload: Record<string, unknown>): Promise<T> {
		const frame = await this.client.request(method, payload, {
			timeoutMs: EXTENSION_HOOK_TIMEOUT_MS,
		});
		return frame.payload as T;
	}

	// -----------------------------------------------------------------------
	// Proxies (Rust owns real session/model state)
	// -----------------------------------------------------------------------

	// Documented escape hatch: these proxies satisfy the structural types
	// required by ExtensionRunner's constructor. Rust owns the real
	// SessionManager/ModelRegistry; the host only needs objects that don't
	// crash when extension code probes them.
	// -----------------------------------------------------------------------
	// Registry snapshot + tool/provider bridges
	// -----------------------------------------------------------------------

	/**
	 * Rebuild ExtensionRunner from the current extension list, rebinding core
	 * actions and capturing provider registrations (first registration wins).
	 */
	private rebuildRunner(cwd: string): void {
		if (this.runtime === undefined) return;
		// Keep captured providers across rebuild: late-loaded extensions register
		// via the live callback before rebuild, and pending is already drained.
		this.runner = new ExtensionRunner(
			this.extensions,
			this.runtime,
			cwd,
			this.createSessionManagerProxy(),
			this.createModelRegistryProxy(),
		);
		this.runner.bindCore(
			this.createExtensionActions(),
			this.createContextActions(),
			{
				registerProvider: (name, config) => {
					if (!this.providers.has(name)) {
						this.providers.set(name, config);
					}
				},
				unregisterProvider: (name) => {
					this.providers.delete(name);
				},
			},
		);
		this.runner.setUIContext(this.createUIContext(), "tui");
		this.runner.onError((error) => {
			this.emitExtensionError(error.extensionPath, error.event, error.error);
		});
	}

	/** Full RegistrySnapshotWire for Rust HostExtensionRunner::load. */
	private buildRegistrySnapshot(): Record<string, unknown> {
		const runner = this.runner;
		if (runner === undefined) {
			return {
				tools: [],
				commands: [],
				shortcuts: [],
				flags: [],
				renderers: [],
				providers: [],
				handlers: [],
			};
		}

		const tools = runner.getAllRegisteredTools().map((tool) => {
			const def = tool.definition;
			const entry: Record<string, unknown> = {
				name: def.name,
				label: def.label,
				description: def.description,
				parameters: def.parameters ?? {},
			};
			if (def.executionMode !== undefined) {
				entry["executionMode"] = def.executionMode;
			}
			return entry;
		});

		const commands = runner.getRegisteredCommands().map((cmd) => ({
			name: cmd.invocationName,
			description: cmd.description,
			source: cmd.sourceInfo.extensionPath,
		}));

		const shortcuts: Array<Record<string, unknown>> = [];
		const seenShortcuts = new Set<string>();
		for (const ext of this.extensions) {
			for (const [key, shortcut] of ext.shortcuts) {
				if (seenShortcuts.has(key)) continue;
				seenShortcuts.add(key);
				shortcuts.push({
					key,
					description: shortcut.description,
				});
			}
		}

		const flagValues = runner.getFlagValues();
		const flags: Array<Record<string, unknown>> = [];
		for (const [name, flag] of runner.getFlags()) {
			const entry: Record<string, unknown> = {
				name,
				description: flag.description,
				type: flag.type,
			};
			if (flag.default !== undefined) {
				entry["default"] = String(flag.default);
			}
			if (flagValues.has(name)) {
				entry["value"] = flagValues.get(name);
			}
			flags.push(entry);
		}

		const renderers: Array<Record<string, unknown>> = [];
		const seenRenderers = new Set<string>();
		for (const ext of this.extensions) {
			for (const name of ext.messageRenderers.keys()) {
				const key = `message:${name}`;
				if (seenRenderers.has(key)) continue;
				seenRenderers.add(key);
				renderers.push({ type: "message", name });
			}
			if (ext.entryRenderers) {
				for (const name of ext.entryRenderers.keys()) {
					const key = `entry:${name}`;
					if (seenRenderers.has(key)) continue;
					seenRenderers.add(key);
					renderers.push({ type: "widget", name });
				}
			}
		}

		const providers: Array<Record<string, unknown>> = [];
		for (const [name, config] of this.providers) {
			const entry: Record<string, unknown> = {
				name,
				streamSimple: typeof config.streamSimple === "function",
			};
			if (config.baseUrl !== undefined) entry["baseUrl"] = config.baseUrl;
			if (config.api !== undefined) entry["api"] = config.api;
			if (config.name !== undefined) entry["displayName"] = config.name;
			if (config.apiKey !== undefined) entry["apiKey"] = config.apiKey;
			if (config.headers !== undefined) entry["headers"] = config.headers;
			if (config.authHeader !== undefined) entry["authHeader"] = config.authHeader;
			if (config.models !== undefined) entry["models"] = config.models;
			providers.push(entry);
		}

		const handlers = ALL_EVENT_TYPES.filter((eventType) => runner.hasHandlers(eventType));

		return { tools, commands, shortcuts, flags, renderers, providers, handlers };
	}

	private handleControlEvent(frame: Frame): void {
		if (frame.method !== "tool.cancel" && frame.method !== "provider.cancel") {
			return;
		}
		const payload = frame.payload as Record<string, unknown>;
		const requestId = typeof payload["id"] === "number" ? payload["id"] : undefined;
		if (requestId === undefined) return;
		this.inFlightTools.get(requestId)?.abort();
		this.inFlightProviders.get(requestId)?.abort();
	}

	private async handleToolPrepare(id: number, p: Record<string, unknown>): Promise<void> {
		const name = String(p["name"] ?? "");
		const args = p["args"];
		const def = this.runner?.getToolDefinition(name);
		if (def === undefined) {
			await this.client.respondError(id, "tool.prepare" as Method, {
				code: "not_found",
				message: `Tool not found: ${name}`,
				retryable: false,
			});
			return;
		}
		try {
			const prepared = def.prepareArguments !== undefined
				? def.prepareArguments(args)
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
		const def = this.runner?.getToolDefinition(name);
		if (def === undefined) {
			await this.client.respondError(id, "tool.validate" as Method, {
				code: "not_found",
				message: `Tool not found: ${name}`,
				retryable: false,
			});
			return;
		}
		// Schema validation lives in the TypeScript tool definition; pass through.
		void def;
		await this.client.respond(id, "tool.validate" as Method, { args });
	}

	private async handleToolExecute(id: number, p: Record<string, unknown>): Promise<void> {
		const name = String(p["name"] ?? "");
		const toolCallId = String(p["toolCallId"] ?? "");
		const args = p["args"];
		const runner = this.runner;
		const def = runner?.getToolDefinition(name);
		if (runner === undefined || def === undefined) {
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
			const prepared = def.prepareArguments !== undefined
				? def.prepareArguments(args)
				: (args as ToolDefinition["parameters"]);
			const result = await def.execute(
				toolCallId,
				prepared,
				controller.signal,
				(partial) => {
					this.client.send({
						id,
						kind: "event",
						method: "toolUpdate",
						payload: {
							toolCallId,
							toolName: name,
							partialResult: partial,
						},
					}).catch(() => void 0);
				},
				runner.createContext(),
			);
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
		const config = this.providers.get(providerId);
		if (config === undefined || typeof config.streamSimple !== "function") {
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
			...((p["options"] as Record<string, unknown> | undefined) ?? {}),
			signal: controller.signal,
		};
		try {
			const stream = config.streamSimple(p["model"], p["context"], options);
			for await (const event of stream) {
				if (controller.signal.aborted) break;
				// Stream-correlated providerEvent carries the AssistantMessageEvent payload.
				await this.client.send({
					id,
					kind: "event",
					method: "providerEvent",
					payload: event,
				});
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

		// Escape hatch: ExtensionActions is a 14-method reference interface; the
	// bridge stub implements the subset extensions use. Rust owns the real state.
	private createExtensionActions(): ExtensionActions {
		return {
			sendMessage: () => {}, sendUserMessage: () => {}, appendEntry: () => {},
			setSessionName: () => {}, getSessionName: () => undefined, setLabel: () => {},
			getActiveTools: () => [], getAllTools: () => [], setActiveTools: () => {},
			refreshTools: () => {}, getCommands: () => [],
			setModel: async () => false,
			getThinkingLevel: () => "medium",
			setThinkingLevel: () => {},
		} as unknown as ExtensionActions;
	}

	// Escape hatch: ExtensionContextActions is an 11-method reference interface.
	private createContextActions(): ExtensionContextActions {
		return {
			getModel: () => undefined, isIdle: () => true,
			isProjectTrusted: () => this.projectTrusted,
			getSignal: () => undefined, abort: () => {}, hasPendingMessages: () => false,
			shutdown: () => {}, getContextUsage: () => undefined, compact: () => {},
			getSystemPrompt: () => "",
		} as unknown as ExtensionContextActions;
	}

	// Escape hatch: SessionManager is a 1600-line reference class; Proxy routes
	// any property access to a no-op. Rust owns the real session state.
	private createSessionManagerProxy(): ConstructorParameters<typeof ExtensionRunner>[3] {
		return new Proxy({}, { get: () => () => undefined }) as unknown as ConstructorParameters<typeof ExtensionRunner>[3];
	}

	// Escape hatch: ModelRegistry wraps a runtime the host doesn't own.
	private createModelRegistryProxy(): ConstructorParameters<typeof ExtensionRunner>[4] {
		return {
			getAll: () => [], getAvailable: () => [], find: () => undefined,
			hasConfiguredAuth: () => false, getProviderDisplayName: (n: string) => n,
			registerProvider: () => {}, unregisterProvider: () => {},
			getRegisteredProviderIds: () => [],
		} as unknown as ConstructorParameters<typeof ExtensionRunner>[4];
	}

	// -----------------------------------------------------------------------
	// Error handling & shutdown
	// -----------------------------------------------------------------------

	private emitExtensionError(path: string, event: string, message: string): void {
		this.client.send({
			id: 0, kind: "event", method: "extensionError",
			payload: {
				code: "extension_error",
				message: `[${path}] ${event}: ${message}`,
				retryable: false,
			},
		}).catch(() => void 0);
	}

	private respondError(frame: Frame, message: string): void {
		this.client.respondError(frame.id, frame.method as Method, {
			code: "extension_error", message, retryable: false,
		}).catch(() => void 0);
	}

	private terminate(message: string): void {
		console.error(`[host] fatal: ${message}`);
		this.dispose(message);
	}

	dispose(reason = "host disposed"): void {
		if (this.state === HostState.DISPOSED) return;
		this.state = HostState.DISPOSED;
		this.terminalHandlers.length = 0;
		this.terminalInputQueue.length = 0;
		for (const controller of this.inFlightTools.values()) controller.abort();
		this.inFlightTools.clear();
		for (const controller of this.inFlightProviders.values()) controller.abort();
		this.inFlightProviders.clear();
		this.providers.clear();
		for (const key of [...this.slots.keys()]) this.disposeSlot(key);
		this.client.dispose(reason);
	}

	get isDisposed(): boolean { return this.state === HostState.DISPOSED; }
	get extensionCount(): number { return this.extensions.length; }
	getExtensions(): Extension[] { return [...this.extensions]; }
	getRunner(): ExtensionRunner | undefined { return this.runner; }
}
