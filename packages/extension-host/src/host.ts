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
	BranchSummaryEntry,
	SessionManagerSetupBridge,
	Extension,
	ExtensionActions,
	ExtensionCommandContext,
	ExtensionCommandContextActions,
	ExtensionContext,
	ExtensionContextActions,
	ExtensionFactory,
	ExtensionRuntime,
	ExtensionUIContext,
	InlineExtension,
	ProviderConfig,
	ReplacedSessionContext,
	ToolDefinition,
	Theme,
} from "@earendil-works/pi-coding-agent";
import type { Context, Model, SimpleStreamOptions } from "@earendil-works/pi-ai";
import { AsyncLocalStorage } from "node:async_hooks";
import { EventEmitter } from "node:events";
import { validateToolArguments } from "@earendil-works/pi-ai/compat";
import { AssistantDeltaReducer } from "./assistant-delta.ts";

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

const STALE_COMMAND_CONTEXT_MESSAGE =
	"This extension ctx is stale after session replacement or reload. Do not use a captured pi or command ctx after ctx.newSession(), ctx.fork(), ctx.switchSession(), or ctx.reload(). For newSession, fork, and switchSession, move post-replacement work into withSession and use the ctx passed to withSession. For reload, do not use the old ctx after await ctx.reload().";

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
type SlotComponent = {
	render(width: number): string[];
	handleInput?(data: string): void;
	dispose?(): void;
	/** Receive a new theme without reconstruction (stateful overlays). */
	updateTheme?(theme: Theme): void;
};

type SlotFactory = (theme: Theme) => unknown;

interface SlotEntry {
	generation: number;
	component: SlotComponent | null;
	placement: SlotPlacement;
	focusable: boolean;
	overlayOptions: OverlayOptions | undefined;
	width: number;
	/** Retained only for components that must capture the active theme at construction. */
	recreate?: SlotFactory;
	/** When false, theme updates re-render without reconstruction (preserves state). */
	recreateOnThemeUpdate: boolean;
	/** Invalidates older asynchronous recreation results. */
	recreationRevision: number;
}

function isSlotComponent(value: unknown): value is SlotComponent {
	return value !== null
		&& typeof value === "object"
		&& typeof (value as SlotComponent).render === "function";
}

/**
 * Structured cancellation only: a real Error (or DOMException, which is
 * not Error-derived in every runtime) named AbortError. Message text is
 * deliberately never consulted — an extension failure that merely says
 * "cancelled" must stay an extension_error.
 */
function isStructuredAbortError(error: unknown): boolean {
	if (error instanceof Error && error.name === "AbortError") return true;
	return typeof DOMException === "function"
		&& error instanceof DOMException
		&& error.name === "AbortError";
}

/** Pending load options captured during the hello handshake. */
interface LoadOptions {
	cwd: string;
	extensionPaths: string[];
	factories: InlineExtension[];
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

const TOOL_RENDER_WIDTH = 80;
const TOOL_RENDER_THEME = {
	name: "extension-host-reference",
	fg: (_color: string, text: string) => text,
	bg: (_color: string, text: string) => text,
	bold: (text: string) => `\x1b[1m${text}\x1b[22m`,
	italic: (text: string) => `\x1b[3m${text}\x1b[23m`,
	underline: (text: string) => `\x1b[4m${text}\x1b[24m`,
	inverse: (text: string) => `\x1b[7m${text}\x1b[27m`,
	strikethrough: (text: string) => `\x1b[9m${text}\x1b[29m`,
	getFgAnsi: (_color: string) => "",
	getBgAnsi: (_color: string) => "",
	getColorMode: () => "truecolor",
	getThinkingBorderColor: (_level: string) => (text: string) => text,
	getBashModeBorderColor: () => (text: string) => text,
} as Theme;
// ---------------------------------------------------------------------------
// Theme bridge (theme.update / theme.set open methods)
// ---------------------------------------------------------------------------

/** One slot value in the theme wire vocabulary: `""`, `"#rrggbb"`, or index. */
type ThemeColorValue = string | number;

/** Resolved theme payload pushed by Rust and sent back for the object form. */
interface ThemeWirePayload {
	name?: string;
	sourcePath?: string;
	colorMode: string;
	fg: Record<string, ThemeColorValue>;
	bg: Record<string, ThemeColorValue>;
}

/** Catalog entry for `getAllThemes` / `getTheme`. */
interface ThemeCatalogEntryPayload {
	name: string;
	path?: string;
	fileStem?: string;
	theme: ThemeWirePayload;
}

/** Mirrored session state pushed by Rust (`session.update`). */
interface SessionStatePayload {
	sessionName?: string;
	thinkingLevel: string;
	activeTools: string[];
	allTools: Array<{ name: string; description: string; parameters: unknown; source?: string }>;
	commands: Array<{ name: string; description?: string; source: string }>;
	model?: Record<string, unknown>;
	/** Models scoped to this session (`--models` / `enabledModels`). */
	scopedModels: Array<Record<string, unknown>>;
	isIdle: boolean;
	hasPendingMessages: boolean;
	contextUsage?: Record<string, unknown>;
	systemPrompt: string;
}

/** Mirror served before the first `session.update` arrives. */
function initialSessionState(): SessionStatePayload {
	return {
		thinkingLevel: "medium",
		activeTools: [],
		allTools: [],
		commands: [],
		scopedModels: [],
		isIdle: true,
		hasPendingMessages: false,
		systemPrompt: "",
	};
}

/** Minimal `SourceInfo` for natively owned tools/commands crossing the bridge. */
function nativeSourceInfo(source: string): Record<string, unknown> {
	return { path: `<${source}>`, source };
}

/** `theme.update` event payload (Rust → host). */
interface ThemeUpdatePayload {
	theme: ThemeWirePayload;
	terminalTheme: "dark" | "light";
	themeMode: string;
	themeGeneration: number;
	themes: ThemeCatalogEntryPayload[];
}

/** Foreground slot names in schema order (mirrors Rust `ALL_FG_SLOTS`). */
const THEME_FG_SLOTS = [
	"accent", "border", "borderAccent", "borderMuted", "success", "error",
	"warning", "muted", "dim", "text", "thinkingText", "userMessageText",
	"customMessageText", "customMessageLabel", "toolTitle", "toolOutput",
	"mdHeading", "mdLink", "mdLinkUrl", "mdCode", "mdCodeBlock",
	"mdCodeBlockBorder", "mdQuote", "mdQuoteBorder", "mdHr", "mdListBullet",
	"toolDiffAdded", "toolDiffRemoved", "toolDiffContext", "syntaxComment",
	"syntaxKeyword", "syntaxFunction", "syntaxVariable", "syntaxString",
	"syntaxNumber", "syntaxType", "syntaxOperator", "syntaxPunctuation",
	"thinkingOff", "thinkingMinimal", "thinkingLow", "thinkingMedium",
	"thinkingHigh", "thinkingXhigh", "thinkingMax", "bashMode",
] as const;

/** Background slot names in schema order (mirrors Rust `ALL_BG_SLOTS`). */
const THEME_BG_SLOTS = [
	"selectedBg", "scrollbarThumb", "userMessageBg", "customMessageBg", "toolPendingBg",
	"toolSuccessBg", "toolErrorBg",
] as const;

function hexToRgbTriple(hex: string): [number, number, number] | undefined {
	if (!/^#[0-9a-fA-F]{6}$/.test(hex)) return undefined;
	return [
		Number.parseInt(hex.slice(1, 3), 16),
		Number.parseInt(hex.slice(3, 5), 16),
		Number.parseInt(hex.slice(5, 7), 16),
	];
}

/** Upstream `fgAnsi` for the wire vocabulary (indices are pre-downsampled). */
function colorAnsi(value: ThemeColorValue, layer: "fg" | "bg"): string {
	const [set, reset] = layer === "fg" ? ["38", "39"] : ["48", "49"];
	if (value === "") return `\x1b[${reset}m`;
	if (typeof value === "number") return `\x1b[${set};5;${value}m`;
	const rgb = hexToRgbTriple(value);
	if (rgb === undefined) return `\x1b[${reset}m`;
	return `\x1b[${set};2;${rgb[0]};${rgb[1]};${rgb[2]}m`;
}

/** Parse a single fg/bg ANSI prefix back into the wire vocabulary. */
function ansiToColorValue(ansi: string, layer: "fg" | "bg"): ThemeColorValue {
	const set = layer === "fg" ? "38" : "48";
	const rgbMatch = new RegExp(`^\\x1b\\[${set};2;(\\d+);(\\d+);(\\d+)m$`).exec(ansi);
	if (rgbMatch !== null) {
		const [r, g, b] = [rgbMatch[1], rgbMatch[2], rgbMatch[3]].map((n) => Number.parseInt(n ?? "0", 10));
		const hex = (n: number | undefined) => (n ?? 0).toString(16).padStart(2, "0");
		return `#${hex(r)}${hex(g)}${hex(b)}`;
	}
	const indexMatch = new RegExp(`^\\x1b\\[${set};5;(\\d+)m$`).exec(ansi);
	if (indexMatch !== null) return Number.parseInt(indexMatch[1] ?? "0", 10);
	return "";
}

const THINKING_LEVEL_SLOTS: Record<string, string> = {
	off: "thinkingOff",
	minimal: "thinkingMinimal",
	low: "thinkingLow",
	medium: "thinkingMedium",
	high: "thinkingHigh",
	xhigh: "thinkingXhigh",
	max: "thinkingMax",
};

/**
 * Construct a reference-shaped `Theme` from wire data. Method behavior
 * mirrors the upstream `Theme` class: `fg`/`bg` wrap text with a
 * color-scoped reset, unknown slots throw, and thinkingMax falls back to
 * thinkingXhigh.
 */
function buildThemeFromWire(wire: ThemeWirePayload): Theme {
	const fgColors = new Map<string, string>();
	for (const slot of THEME_FG_SLOTS) {
		const value = wire.fg[slot] ?? (slot === "thinkingMax" ? wire.fg["thinkingXhigh"] : undefined);
		if (value !== undefined) fgColors.set(slot, colorAnsi(value, "fg"));
	}
	const bgColors = new Map<string, string>();
	for (const slot of THEME_BG_SLOTS) {
		const value = wire.bg[slot];
		if (value !== undefined) bgColors.set(slot, colorAnsi(value, "bg"));
	}
	const getFgAnsi = (color: string): string => {
		const ansi = fgColors.get(color);
		if (ansi === undefined) throw new Error(`Unknown theme color: ${color}`);
		return ansi;
	};
	const getBgAnsi = (color: string): string => {
		const ansi = bgColors.get(color);
		if (ansi === undefined) throw new Error(`Unknown theme background color: ${color}`);
		return ansi;
	};
	const fg = (color: string, text: string) => `${getFgAnsi(color)}${text}\x1b[39m`;
	const theme = {
		name: wire.name,
		sourcePath: wire.sourcePath,
		fg,
		bg: (color: string, text: string) => `${getBgAnsi(color)}${text}\x1b[49m`,
		bold: (text: string) => `\x1b[1m${text}\x1b[22m`,
		italic: (text: string) => `\x1b[3m${text}\x1b[23m`,
		underline: (text: string) => `\x1b[4m${text}\x1b[24m`,
		inverse: (text: string) => `\x1b[7m${text}\x1b[27m`,
		strikethrough: (text: string) => `\x1b[9m${text}\x1b[29m`,
		getFgAnsi,
		getBgAnsi,
		getColorMode: () => (wire.colorMode === "256color" ? "256color" : "truecolor"),
		getThinkingBorderColor: (level: string) => (text: string) =>
			fg(THINKING_LEVEL_SLOTS[level] ?? "thinkingOff", text),
		getBashModeBorderColor: () => (text: string) => fg("bashMode", text),
	};
	return theme as Theme;
}

/** Reset-only theme served before the first `theme.update` arrives. */
function fallbackTheme(): Theme {
	const fg: Record<string, ThemeColorValue> = {};
	for (const slot of THEME_FG_SLOTS) fg[slot] = "";
	const bg: Record<string, ThemeColorValue> = {};
	for (const slot of THEME_BG_SLOTS) bg[slot] = "";
	return buildThemeFromWire({ name: "dark", colorMode: "truecolor", fg, bg });
}

/**
 * Serialize an extension-supplied `Theme` instance into wire form via its
 * public accessors. Throws when the object is not Theme-shaped.
 */
function serializeThemeInstance(theme: Theme): ThemeWirePayload {
	const fg: Record<string, ThemeColorValue> = {};
	for (const slot of THEME_FG_SLOTS) {
		try {
			fg[slot] = ansiToColorValue(theme.getFgAnsi(slot), "fg");
		} catch {
			// Upstream themes may omit optional slots (e.g. thinkingMax).
		}
	}
	const bg: Record<string, ThemeColorValue> = {};
	for (const slot of THEME_BG_SLOTS) {
		try {
			bg[slot] = ansiToColorValue(theme.getBgAnsi(slot), "bg");
		} catch {
			// Optional slot; leave empty.
		}
	}
	if (Object.keys(fg).length === 0) {
		throw new Error("theme object exposes no foreground colors");
	}
	// `sourcePath` exists on the upstream Theme class but not on the
	// re-exported structural type; probe it without asserting a new shape.
	const sourcePath = "sourcePath" in theme ? theme.sourcePath : undefined;
	return {
		name: typeof theme.name === "string" ? theme.name : undefined,
		sourcePath: typeof sourcePath === "string" ? sourcePath : undefined,
		colorMode: theme.getColorMode(),
		fg,
		bg,
	};
}

/** Upstream `parseAutoThemeSetting`: exactly one `/`, both members non-empty. */
function parseThemePair(raw: string): { lightTheme: string; darkTheme: string } | undefined {
	const slashIndex = raw.indexOf("/");
	if (slashIndex === -1 || raw.indexOf("/", slashIndex + 1) !== -1) return undefined;
	const lightTheme = raw.slice(0, slashIndex).trim();
	const darkTheme = raw.slice(slashIndex + 1).trim();
	if (lightTheme === "" || darkTheme === "") return undefined;
	return { lightTheme, darkTheme };
}

function escapeHtml(text: string): string {
	return text
		.replaceAll("&", "&amp;")
		.replaceAll("<", "&lt;")
		.replaceAll(">", "&gt;")
		.replaceAll('\"', "&quot;")
		.replaceAll("'", "&#39;");
}

function ansiToInertHtml(lines: string[]): string {
	const rendered = parseAnsiLines(lines.join("\n"))
		.map((line) => line.map((run) => {
			let text = escapeHtml(run.text);
			if (run.style?.strikethrough) text = `<s>${text}</s>`;
			if (run.style?.underline) text = `<u>${text}</u>`;
			if (run.style?.italic) text = `<em>${text}</em>`;
			if (run.style?.bold) text = `<strong>${text}</strong>`;
			return text;
		}).join(""))
		.join("\n");
	return `<pre class="pi-tool-render">${rendered}</pre>`;
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
	private hasLoadedProtocolExtensions = false;
	/** Captured custom providers (first registration wins). */
	private readonly providers = new Map<string, ProviderConfig>();
	/** In-flight tool.execute AbortControllers keyed by request id. */
	private readonly inFlightTools = new Map<number, AbortController>();
	/** In-flight provider.stream AbortControllers keyed by request id. */
	private readonly inFlightProviders = new Map<number, AbortController>();
	/** Active shortcut handlers keyed by their resolved shortcut key (single-flight). */
	private readonly inFlightShortcuts = new Map<string, AbortController>();
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
	/** Compact assistant stream reconstruction (shared with the lean runner). */
	private readonly assistantDelta = new AssistantDeltaReducer();
	/** Active theme served to extensions (`ctx.ui.theme`). */
	private currentTheme: Theme = fallbackTheme();
	/** Theme catalog from the latest `theme.update` push. */
	private themeCatalog: ThemeCatalogEntryPayload[] = [];
	/** Detected terminal polarity from the latest `theme.update` push. */
	private terminalTheme: "dark" | "light" = "dark";
	/** Active `themeMode` from the latest `theme.update` push. */
	private themeMode = "auto";
	/** Mirrored session state behind the synchronous session getters. */
	private sessionState: SessionStatePayload = initialSessionState();
	/** Mirrored UI state behind `getEditorText` / `getToolsExpanded`. */
	private uiState = { editorText: "", toolsExpanded: false };
	/** Abort controller for the current agent turn (`ctx.getSignal`). */
	private turnAbort: AbortController | undefined;
	/** Resolvers waiting for the next idle transition (`ctx.waitForIdle`). */
	private readonly idleWaiters: Array<{ resolve: () => void; reject: (reason?: unknown) => void }> = [];
	/** Extension statuses set via `ui.setStatus` (served to custom footers). */
	private readonly extensionStatuses = new Map<string, string>();
	/**
	 * Per-command.execute replacement-token scope. Tokens are captured from
	 * ready-gated session responses and emitted as `session.replacementReady`
	 * only from that command's finally path — never via a global slot.
	 */
	private readonly commandScope = new AsyncLocalStorage<{ tokens: string[]; closed: boolean }>();

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
		factories?: InlineExtension[];
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

		for (const [index, input] of opts.factories.entries()) {
			const isNamed = typeof input !== "function";
			const factory = isNamed ? input.factory : input;
			const extensionPath = `<inline:${isNamed ? input.name : index + 1}>`;
			try {
				const ext = await loadExtensionFromFactory(
					factory, opts.cwd, eventBus, this.runtime, extensionPath,
				);
				ext.hidden = isNamed ? input.hidden ?? false : false;
				this.extensions.push(ext);
			} catch (err) {
				errors.push({
					path: extensionPath,
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
			case "flags.set":
				await this.handleFlagsSet(id, p);
				return;
			case "shortcut.execute":
				await this.handleShortcutExecute(id, p);
				return;
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
			case "tool.renderHtml":
				await this.handleToolRenderHtml(id, p);
				return;
			case "message_update_delta":
				await this.handleMessageUpdateDelta(id, p);
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

	private async handleFlagsSet(id: number, p: Record<string, unknown>): Promise<void> {
		const runner = this.runner;
		const values = p["values"];
		if (runner === undefined) {
			await this.client.respondError(id, "flags.set", {
				code: "extension_error", message: "runner not initialized", retryable: false,
			});
			return;
		}
		if (!isRecord(values)) {
			await this.client.respondError(id, "flags.set", {
				code: "invalid_arguments", message: "flags.set values must be an object", retryable: false,
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
			runner.setFlagValue(name, value);
		}
		await this.client.respond(id, "flags.set", { ok: true });
	}

	private async handleShortcutExecute(id: number, p: Record<string, unknown>): Promise<void> {
		const key = p["key"];
		const runner = this.runner;
		if (typeof key !== "string" || runner === undefined) {
			await this.client.respond(id, "shortcut.execute", { handled: false });
			return;
		}
		let shortcut: (Extension["shortcuts"] extends Map<string, infer T> ? T : never) | undefined;
		for (let index = this.extensions.length - 1; index >= 0; index--) {
			const candidate = this.extensions[index]?.shortcuts.get(key);
			if (candidate !== undefined) {
				shortcut = candidate;
				break;
			}
		}

		if (shortcut === undefined) {
			await this.client.respond(id, "shortcut.execute", { handled: false });
			return;
		}

		const active = this.inFlightShortcuts.get(key);
		if (active !== undefined) {
			await this.client.respond(id, "shortcut.execute", { handled: true });
			return;
		}
		const controller = new AbortController();
		this.inFlightShortcuts.set(key, controller);
		try {
			await this.client.respond(id, "shortcut.execute", { handled: true });
		} catch (error) {
			if (this.inFlightShortcuts.get(key) === controller) {
				this.inFlightShortcuts.delete(key);
			}
			throw error;
		}
		// Hand the handler a context whose `signal` is THIS invocation's
		// cancellation (aborted on dispose / keyed single-flight), matching the
		// lean runner's shortcut context. defineProperties keeps the runner's
		// other getters lazy instead of freezing eager reads into a spread.
		const context = Object.defineProperties(
			{},
			Object.getOwnPropertyDescriptors(runner.createContext()),
		) as ExtensionContext;
		Object.defineProperty(context, "signal", {
			get: () => controller.signal,
			enumerable: true,
		});
		void Promise.resolve()
			.then(() => shortcut.handler(context))
			.catch((error) => {
				if (controller.signal.aborted || this.state === HostState.DISPOSED) {
					return;
				}
				this.emitExtensionError(
					shortcut.extensionPath,
					"shortcut.execute",
					error instanceof Error ? error.message : String(error),
				);
			})
			.finally(() => {
				if (this.inFlightShortcuts.get(key) === controller) {
					this.inFlightShortcuts.delete(key);
				}
			});
	}

	/**
	 * Drive the real ExtensionRunner for a lifecycle hook. The method name IS
	 * the event type discriminant; response shaping mirrors LeanRunner / the
	 * specialized upstream emitters (before_agent_start, tool_call, tool_result,
	 * message_end) rather than the generic emit() discard path.
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
			if (eventType === "message_start") {
				const message = payload["message"];
				if (isRecord(message) && message["role"] === "assistant") {
					this.assistantDelta.seedActiveAssistant(message);
				}
			}
			let result: unknown;
			switch (eventType) {
				case "before_agent_start": {
					const cwd = this.loadOptions?.cwd ?? process.cwd();
					const systemPrompt =
						typeof payload["systemPrompt"] === "string"
							? payload["systemPrompt"]
							: this.sessionState.systemPrompt;
					const combined = await runner.emitBeforeAgentStart(
						typeof payload["prompt"] === "string" ? payload["prompt"] : "",
						payload["images"] as Parameters<typeof runner.emitBeforeAgentStart>[1],
						systemPrompt,
						{ cwd },
					);
					const response: Record<string, unknown> = {};
					if (combined?.messages !== undefined && combined.messages.length > 0) {
						response["messages"] = combined.messages;
					}
					if (combined?.systemPrompt !== undefined) {
						response["systemPrompt"] = combined.systemPrompt;
					}
					await this.client.respond(id, eventType as Method, response);
					return;
				}
				case "tool_call": {
					const input = payload["input"];
					if (!isRecord(input)) throw new Error("tool_call.input is required");
					const baseline = structuredClone(input);
					result = await runner.emitToolCall({
						type: "tool_call",
						toolName: payload["toolName"] as string,
						toolCallId: payload["toolCallId"] as string,
						input,
					});
					const response: Record<string, unknown> = {
						...(isRecord(result) ? result : {}),
					};
					if (!canonicalJsonEqual(input, baseline)) {
						response["input"] = input;
					} else {
						delete response["input"];
					}
					await this.client.respond(id, eventType as Method, response);
					return;
				}
				case "tool_result": {
					const input = payload["input"];
					if (!isRecord(input)) throw new Error("tool_result.input is required");
					result = await runner.emitToolResult({
						type: "tool_result",
						toolName: payload["toolName"] as string,
						toolCallId: payload["toolCallId"] as string,
						input,
						content: payload["content"] as never,
						details: payload["details"],
						isError: payload["isError"] === true,
					});
					const response: Record<string, unknown> = {};
					if (isRecord(result)) {
						if (result["content"] !== undefined) response["content"] = result["content"];
						if (result["details"] !== undefined) response["details"] = result["details"];
						if (result["isError"] !== undefined) response["isError"] = result["isError"];
						if (result["usage"] !== undefined) response["usage"] = result["usage"];
					}
					await this.client.respond(id, eventType as Method, response);
					return;
				}
				case "message_end": {
					this.assistantDelta.clearActiveAssistant();
					// Rust sends the raw AgentMessage AS the request payload (no
					// `{ message }` wrapper); wrap it for emitMessageEnd.
					result = await runner.emitMessageEnd({
						type: "message_end",
						message: payload as never,
					});
					await this.client.respond(id, eventType as Method, {
						message: result ?? undefined,
					});
					return;
				}
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
					if (eventType === "agent_end" || eventType === "session_shutdown") {
						this.assistantDelta.clearActiveAssistant();
					}
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

	private async handleMessageUpdateDelta(
		id: number, payload: Record<string, unknown>,
	): Promise<void> {
		const runner = this.runner;
		if (runner === undefined) {
			await this.client.respondError(id, "message_update_delta" as Method, {
				code: "extension_error", message: "runner not initialized", retryable: false,
			});
			return;
		}
		const event = payload["event"];
		if (!isRecord(event) || typeof event["type"] !== "string") {
			await this.client.respondError(id, "message_update_delta" as Method, {
				code: "invalid_request", message: "message_update_delta.event is required", retryable: false,
			});
			return;
		}

		try {
			const type = event["type"] as string;
			const final = event["final"];
			if ((type === "done" || type === "error") && isRecord(final)) {
				const message = structuredClone(final);
				const assistantMessageEvent = type === "done"
					? { type, reason: event["reason"], message }
					: { type, reason: event["reason"], error: message };
				this.assistantDelta.clearActiveAssistant();
				const result = await runner.emit({
					type: "message_update",
					message,
					assistantMessageEvent,
				} as Parameters<typeof runner.emit>[0]);
				await this.client.respond(id, "message_update_delta" as Method, result ?? { ok: true });
				return;
			}

			this.assistantDelta.applyAssistantDelta(event);
			const activeAssistant = this.assistantDelta.getActiveAssistant();
			if (activeAssistant === undefined) {
				throw new Error("message update arrived before assistant start");
			}
			const message = structuredClone(activeAssistant);
			const assistantMessageEvent = this.assistantDelta.expandAssistantEvent(event, message);
			const result = await runner.emit({
				type: "message_update",
				message,
				assistantMessageEvent,
			} as Parameters<typeof runner.emit>[0]);
			await this.client.respond(id, "message_update_delta" as Method, result ?? { ok: true });
		} catch (error) {
			await this.client.respondError(id, "message_update_delta" as Method, {
				code: "extension_error",
				message: error instanceof Error ? error.message : String(error),
				retryable: false,
			});
		}
	}

	private async handleExtensionsLoad(id: number, p: Record<string, unknown>): Promise<void> {
		this.assistantDelta.clearActiveAssistant();
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
		const loadedExtensions: Extension[] = [];

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
				loadedExtensions.push(ext);
				loadedCount++;
			} catch (err) {
				errors.push({
					path: extPath,
					error: err instanceof Error ? err.message : String(err),
				});
			}
		}
		// Idempotent per resolved extension path: replace a previously
		// loaded extension for the same path in place (preserving its
		// ordering position) rather than appending a duplicate. New paths
		// keep the first-load-unshift / later-load-push ordering semantics.
		const existingByPath = new Map<string, number>();
		for (const [i, existing] of this.extensions.entries()) {
			if (!existingByPath.has(existing.path)) {
				existingByPath.set(existing.path, i);
			}
		}
		const newExtensions: Extension[] = [];
		const newPathIndex = new Map<string, number>();
		for (const ext of loadedExtensions) {
			const existingIdx = existingByPath.get(ext.path);
			if (existingIdx !== undefined) {
				this.extensions[existingIdx] = ext;
				continue;
			}
			const batchIdx = newPathIndex.get(ext.path);
			if (batchIdx !== undefined) {
				newExtensions[batchIdx] = ext;
			} else {
				newPathIndex.set(ext.path, newExtensions.length);
				newExtensions.push(ext);
			}
		}
		if (newExtensions.length > 0) {
			if (this.hasLoadedProtocolExtensions) {
				this.extensions.push(...newExtensions);
			} else {
				// The first JSONL load is startup configuration, equivalent to CLI
				// extension paths, which load before built-in factories.
				this.extensions.unshift(...newExtensions);
				this.hasLoadedProtocolExtensions = true;
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

		const runner = this.runner;
		const cmd = runner?.getCommand(commandName);
		if (!runner || !cmd) {
			await this.client.respondError(id, "command.execute" as Method, {
				code: "not_found", message: `Command not found: ${commandName}`, retryable: false,
			});
			return;
		}

		const scope = { tokens: [] as string[], closed: false };
		try {
			await this.commandScope.run(scope, async () => {
				await cmd.handler(args, this.createCommandContext(runner));
			});
			await this.client.respond(id, "command.execute" as Method, { ok: true });
		} catch (err) {
			await this.client.respondError(id, "command.execute" as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		} finally {
			// After the command.execute res/error write: writeChain orders ready
			// after the response and any session.command frames from the handler.
			// Emit even when the handler threw after a successful replacement.
			// Closed first so a late fire-and-forget capture is diagnosed, not
			// pushed into a scope nobody will flush.
			scope.closed = true;
			for (const token of scope.tokens) {
				try {
					await this.client.send({
						id: 0,
						kind: "event",
						method: "session.replacementReady",
						payload: { token },
					});
				} catch (err) {
					this.emitExtensionError(
						"<host>",
						"session.replacementReady",
						err instanceof Error ? err.message : String(err),
					);
				}
			}
		}
	}

	private async handleMeasure(id: number, p: Record<string, unknown>): Promise<void> {
		const key = p["key"] as string;
		const width = p["width"] as number;
		const entry = this.slots.get(key);
		if (entry !== undefined && Number.isFinite(width)) entry.width = width;
		const height = entry?.component?.render(width)?.length ?? 0;
		await this.client.respond(id, "measure", { height });
	}

	private async handleRender(id: number, p: Record<string, unknown>): Promise<void> {
		const key = p["key"] as string;
		const width = p["width"] as number;
		const entry = this.slots.get(key);
		if (entry !== undefined && Number.isFinite(width)) entry.width = width;
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

	private async handleUiEvent(id: number, p: Record<string, unknown>): Promise<void> {
		const key = p["key"];
		const generation = p["generation"];
		const event = p["event"];
		const entry = typeof key === "string" ? this.slots.get(key) : undefined;
		if (entry === undefined || entry.generation !== generation || !isRecord(event)) {
			await this.client.respond(id, "uiEvent", { delivered: false });
			return;
		}

		const type = event["type"];
		if (type === "key" || type === "paste") {
			const data = p["data"];
			if (typeof data === "string") entry.component?.handleInput?.(data);
			if (this.slots.get(key as string) === entry) this.pushSlot(key as string, entry, entry.width);
		} else if (type === "resize") {
			const width = event["width"];
			if (typeof width === "number" && Number.isFinite(width)) entry.width = width;
			this.pushSlot(key as string, entry, entry.width);
		} else if (type !== "focusGained" && type !== "focusLost") {
			await this.client.respond(id, "uiEvent", { delivered: false });
			return;
		}

		await this.client.respond(id, "uiEvent", { delivered: true });
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

	/**
	 * Install (or clear, when `content` is `undefined`) a keyed slot from
	 * either a static `string[]` or an upstream component factory. Factories
	 * receive the host TUI shim and the active theme; footer factories also
	 * receive a `ReadonlyFooterDataProvider` backed by the statuses this host
	 * observed via `ui.setStatus` (git branch / provider count are native
	 * data the headless host does not own).
	 */
	private setComponentSlot(key: string, content: unknown, placement: SlotPlacement): void {
		if (content === undefined || content === null) {
			this.disposeSlot(key);
			return;
		}
		let component: SlotComponent | null;
		let recreate: SlotFactory | undefined;
		if (Array.isArray(content)) {
			const lines = content as string[];
			component = { render: () => lines };
		} else if (typeof content === "function") {
			const tui = {
				requestRender: () => {
					const entry = this.slots.get(key);
					if (entry !== undefined) this.pushSlot(key, entry, entry.width);
				},
			};
			const footerData = {
				getGitBranch: () => null,
				getExtensionStatuses: () => this.extensionStatuses as ReadonlyMap<string, string>,
				getAvailableProviderCount: () => 0,
				onBranchChange: () => () => {},
			};
			recreate = (theme) => (content as (...args: unknown[]) => unknown)(tui, theme, footerData);
			// One factory contract on both paths: store the entry first, then let
			// `recreateSlot` install — it already validates the result, handles
			// thenables, and keeps a synchronous result's slot identical to the
			// previous eager call.
			component = null;
		} else {
			return;
		}
		this.slots.get(key)?.component?.dispose?.();
		const entry: SlotEntry = {
			generation: this.nextGeneration++,
			component,
			placement,
			focusable: false,
			overlayOptions: undefined,
			width: 80,
			recreate,
			recreateOnThemeUpdate: true,
			recreationRevision: 0,
		};
		this.slots.set(key, entry);
		if (entry.recreate === undefined) {
			this.pushSlot(key, entry, 80);
			return;
		}
		this.recreateSlot(key, entry, () => {
			if (this.slots.get(key) === entry) this.disposeSlot(key);
		});
	}

	/**
	 * Re-render a static slot, or recreate a factory-backed slot against
	 * `currentTheme`. The old component stays installed until replacement
	 * succeeds; failures emit an extension error and leave the slot untouched.
	 * Asynchronous recreations carry a monotonic revision so stale results
	 * dispose themselves instead of resurrecting overwritten UI.
	 */
	private recreateSlot(key: string, entry: SlotEntry, onInitialFailure?: () => void): void {
		if (entry.recreate === undefined) {
			this.pushSlot(key, entry, entry.width);
			return;
		}
		const revision = ++entry.recreationRevision;
		const isCurrent = () => this.slots.get(key) === entry && entry.recreationRevision === revision;
		const fail = (err: unknown) => {
			if (!isCurrent()) return;
			this.emitExtensionError("<inline>", "theme.update", String(err));
			onInitialFailure?.();
		};
		const install = (replacement: unknown) => {
			if (!isCurrent()) {
				if (isSlotComponent(replacement)) replacement.dispose?.();
				return;
			}
			if (!isSlotComponent(replacement)) {
				fail(`factory for ${key} did not return a component`);
				return;
			}
			const old = entry.component;
			entry.component = replacement;
			old?.dispose?.();
			this.pushSlot(key, entry, entry.width);
		};
		let recreated: unknown;
		try {
			recreated = entry.recreate(this.currentTheme);
		} catch (err) {
			fail(err);
			return;
		}
		if (recreated !== null && typeof recreated === "object" && typeof (recreated as PromiseLike<unknown>).then === "function") {
			Promise.resolve(recreated).then(install, fail);
			return;
		}
		install(recreated);
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
			setStatus: (key: string, text: string | undefined) => {
				if (text === undefined) {
					self.extensionStatuses.delete(key);
				} else {
					self.extensionStatuses.set(key, text);
				}
				self.sendUiControl({ control: "setStatus", key, text });
			},
			setWorkingMessage: (message?: string) => {
				self.sendUiControl({ control: "setWorkingMessage", message });
			},
			setWorkingVisible: (visible: boolean) => {
				self.sendUiControl({ control: "setWorkingVisible", visible });
			},
			setWorkingIndicator: (options?: unknown) => {
				self.sendUiControl({ control: "setWorkingIndicator", options });
			},
			setHiddenThinkingLabel: (label?: string) => {
				self.sendUiControl({ control: "setHiddenThinkingLabel", label });
			},
			setWidget: (key: string, content: unknown, options?: { placement?: string }) => {
				const placement: SlotPlacement =
					options?.placement === "belowEditor" ? "belowEditor" : "aboveEditor";
				self.setComponentSlot(key, content, placement);
			},
			setFooter: (factory: unknown) => {
				self.setComponentSlot("footer.extension", factory, "footer");
			},
			setHeader: (factory: unknown) => {
				self.setComponentSlot("header.extension", factory, "header");
			},
			setTitle: (title: string) => {
				self.sendUiControl({ control: "setTitle", title });
			},
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
				const tui = {
					requestRender: () => {
						const entry = self.slots.get(key);
						if (entry !== undefined) self.pushSlot(key, entry, entry.width);
					},
				};
				let overlayOptions: OverlayOptions | undefined;
				try {
					overlayOptions = (typeof options?.overlayOptions === "function"
						? options.overlayOptions()
						: options?.overlayOptions) ?? {};
				} catch (err) {
					self.emitExtensionError("<inline>", "custom", String(err));
					done(undefined);
					return promise;
				}
				const entry: SlotEntry = {
					generation: self.nextGeneration++,
					component: null,
					placement: "overlay",
					focusable: true,
					overlayOptions,
					width: 80,
					recreate: (theme) => factory(tui, theme, {}, done),
					recreateOnThemeUpdate: false,
					recreationRevision: 0,
				};
				self.slots.set(key, entry);
				self.recreateSlot(key, entry, () => done(undefined));
				return promise;
			},
			editor: (title: string, prefill?: string) =>
				self.dialog<{ value?: string | null }>("editor", { title, prefill })
					.then((r) => r.value ?? undefined),
			pasteToEditor: (text: string) => {
				self.uiState.editorText += text;
				self.sendUiControl({ control: "pasteToEditor", text });
			},
			setEditorText: (text: string) => {
				self.uiState.editorText = text;
				self.sendUiControl({ control: "setEditorText", text });
			},
			getEditorText: () => self.uiState.editorText,
			// Not portable: autocomplete stacking and editor-component
			// replacement require an interactive editor protocol the bridge
			// does not carry (documented divergence; ledgered by the audit).
			addAutocompleteProvider: () => {},
			setEditorComponent: () => {},
			getEditorComponent: () => undefined,
			get theme() {
				return self.currentTheme;
			},
			getAllThemes: () =>
				self.themeCatalog.map((entry) => ({ name: entry.name, path: entry.path })),
			getTheme: (name: string) => {
				const entry = self.findThemeEntry(name);
				return entry === undefined ? undefined : buildThemeFromWire(entry.theme);
			},
			setTheme: (themeOrName: string | Theme) => self.setThemeFromExtension(themeOrName),
			getToolsExpanded: () => self.uiState.toolsExpanded,
			setToolsExpanded: (expanded: boolean) => {
				self.uiState.toolsExpanded = expanded;
				self.sendUiControl({ control: "setToolsExpanded", expanded });
			},
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
		const runner = new ExtensionRunner(
			this.extensions,
			this.runtime,
			cwd,
			this.createSessionManagerProxy(),
			this.createModelRegistryProxy(),
		);
		runner.bindCore(
			this.createExtensionActions(),
			this.createContextActions(),
			{
				registerProvider: (name, config) => {
					if (!this.providers.has(name)) {
						this.providers.set(name, config);
					}
				},
				registerNativeProvider: (provider) => {
					const id = (provider as Record<string, unknown>)["id"];
					if (typeof id === "string" && !this.providers.has(id)) {
						const native = provider as ProviderConfig;
						this.providers.set(id, {
							name: native.name,
							baseUrl: native.baseUrl,
							streamSimple: native.streamSimple,
						});
					}
				},
				unregisterProvider: (name) => {
					this.providers.delete(name);
				},
			},
		);
		runner.bindCommandContext(this.createCommandContextActions(runner));
		runner.setUIContext(this.createUIContext(), "tui");
		runner.onError((error) => {
			this.emitExtensionError(error.extensionPath, error.event, error.error);
		});
		this.runner = runner;
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
			source: cmd.sourceInfo.path,
			sourceInfo: cmd.sourceInfo,
		}));

		const shortcuts: Array<Record<string, unknown>> = [];
		for (const ext of this.extensions) {
			for (const [key, shortcut] of ext.shortcuts) {
				shortcuts.push({
					key,
					description: shortcut.description,
					extensionPath: shortcut.extensionPath,
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
				extensionPath: flag.extensionPath,
			};
			if (flag.default !== undefined) {
				entry["default"] = flag.default;
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

		return {
			tools, commands, shortcuts, flags, renderers, providers, handlers,
			terminalInput: this.activeTerminalHandlerCount > 0,
		};
	}

	private handleControlEvent(frame: Frame): void {
		if (frame.method === "theme.update") {
			this.applyThemeUpdate(frame.payload as ThemeUpdatePayload);
			return;
		}
		if (frame.method === "session.update") {
			this.applySessionUpdate(frame.payload as Partial<SessionStatePayload>);
			return;
		}
		if (frame.method === "ui.state") {
			const payload = frame.payload as Record<string, unknown>;
			if (typeof payload["editorText"] === "string") {
				this.uiState.editorText = payload["editorText"];
			}
			if (typeof payload["toolsExpanded"] === "boolean") {
				this.uiState.toolsExpanded = payload["toolsExpanded"];
			}
			return;
		}
		if (frame.method !== "tool.cancel" && frame.method !== "provider.cancel") {
			return;
		}
		const payload = frame.payload as Record<string, unknown>;
		const requestId = typeof payload["id"] === "number" ? payload["id"] : undefined;
		if (requestId === undefined) return;
		this.inFlightTools.get(requestId)?.abort();
		this.inFlightProviders.get(requestId)?.abort();
	}

	/**
	 * Apply an authoritative `session.update` push: the synchronous session
	 * getters (`getSessionName`, `isIdle`, `getActiveTools`, …) serve the
	 * latest mirror. An idle→busy transition arms a fresh turn abort
	 * controller so `ctx.getSignal()` tracks the current agent run.
	 */
	private applySessionUpdate(update: Partial<SessionStatePayload>): void {
		if (update === null || typeof update !== "object") return;
		const wasIdle = this.sessionState.isIdle;
		// Rust pushes complete snapshots; optional fields are OMITTED when
		// cleared (serde skip_serializing_if), so absent keys must reset to
		// their defaults — never survive from the previous mirror.
		this.sessionState = { ...initialSessionState(), ...update };
		if (wasIdle && !this.sessionState.isIdle) {
			this.turnAbort = new AbortController();
		}
		if (this.sessionState.isIdle && this.idleWaiters.length > 0) {
			const waiters = this.idleWaiters.splice(0);
			for (const waiter of waiters) waiter.resolve();
		}
	}

	/**
	 * Apply an authoritative `theme.update` push: refresh `ctx.ui.theme`, the
	 * catalog, polarity context, and recreate/re-render every live slot with
	 * the new colors. Factory-backed slots rebuild against `currentTheme`;
	 * static slots only re-render.
	 */
	private applyThemeUpdate(update: ThemeUpdatePayload): void {
		if (update === null || typeof update !== "object" || typeof update.theme !== "object") {
			return;
		}
		this.currentTheme = buildThemeFromWire(update.theme);
		this.themeCatalog = Array.isArray(update.themes) ? update.themes : [];
		this.terminalTheme = update.terminalTheme === "light" ? "light" : "dark";
		this.themeMode = typeof update.themeMode === "string" ? update.themeMode : "auto";
		for (const [key, entry] of this.slots) {
			if (!entry.recreateOnThemeUpdate) {
				// Stateful overlays keep their instance and state; propagate the
				// new theme for components that opt in, then re-render in place.
				entry.component?.updateTheme?.(this.currentTheme);
				this.pushSlot(key, entry, entry.width);
				continue;
			}
			this.recreateSlot(key, entry);
		}
	}

	/** Catalog lookup by display name or custom-theme file stem. */
	private findThemeEntry(name: string): ThemeCatalogEntryPayload | undefined {
		return (
			this.themeCatalog.find((entry) => entry.name === name)
			?? this.themeCatalog.find((entry) => entry.fileStem === name)
		);
	}

	/**
	 * `ctx.ui.setTheme` bridge. Mirrors the upstream theme-controller
	 * semantics: the string form (plain name or `light/dark` pair) persists on
	 * success and falls back to dark on failure without persisting; the
	 * `Theme`-object form applies without persistence.
	 */
	private setThemeFromExtension(
		themeOrName: string | Theme,
	): { success: boolean; error?: string } {
		if (typeof themeOrName !== "string") {
			let wire: ThemeWirePayload;
			try {
				wire = serializeThemeInstance(themeOrName);
			} catch (err) {
				return {
					success: false,
					error: err instanceof Error ? err.message : String(err),
				};
			}
			this.currentTheme = themeOrName;
			this.sendThemeSet({ theme: wire, persist: false });
			return { success: true };
		}

		const pair = parseThemePair(themeOrName);
		const wantDark =
			this.themeMode === "dark"
			|| (this.themeMode === "auto" && this.terminalTheme === "dark");
		const member = pair === undefined
			? themeOrName
			: (wantDark ? pair.darkTheme : pair.lightTheme);
		const entry = this.findThemeEntry(member);
		if (entry === undefined) {
			// Upstream applyThemeName failure: fall back to dark, do not persist.
			const dark = this.findThemeEntry("dark");
			if (dark !== undefined) {
				this.currentTheme = buildThemeFromWire(dark.theme);
				this.sendThemeSet({ name: "dark", persist: false });
			}
			return { success: false, error: `Theme not found: ${member}` };
		}
		this.currentTheme = buildThemeFromWire(entry.theme);
		this.sendThemeSet({ name: themeOrName, persist: true });
		return { success: true };
	}

	/** Fire-and-forget `theme.set` event to Rust (applies + persists there). */
	private sendThemeSet(payload: {
		name?: string;
		theme?: ThemeWirePayload;
		persist: boolean;
	}): void {
		this.client.send({
			id: 0, kind: "event", method: "theme.set" as Method, payload,
		}).catch(() => void 0);
	}

	/** Bridge a `session.command` action to Rust; awaits the wire write. */
	private async sendSessionCommand(payload: Record<string, unknown>): Promise<void> {
		await this.client.send({
			id: 0, kind: "event", method: "session.command" as Method, payload,
		});
	}

	/** Fire-and-forget `ui.control` data-surface control to Rust. */
	private sendUiControl(payload: Record<string, unknown>): void {
		this.client.send({
			id: 0, kind: "event", method: "ui.control" as Method, payload,
		}).catch(() => void 0);
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
		try {
			const validated = validateToolArguments(
				def as Parameters<typeof validateToolArguments>[0],
				{
					type: "toolCall",
					id: String(p["toolCallId"] ?? `validate-${id}`),
					name,
					arguments: args,
				} as Parameters<typeof validateToolArguments>[1],
			);
			await this.client.respond(id, "tool.validate" as Method, { args: validated });
		} catch (err) {
			await this.client.respondError(id, "tool.validate" as Method, {
				code: "invalid_arguments",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		}
	}

	private async handleToolRenderHtml(id: number, p: Record<string, unknown>): Promise<void> {
		const name = String(p["toolName"] ?? "");
		const phase = p["phase"];
		const payload = p["payload"];
		const def = this.runner?.getToolDefinition(name);
		if (def === undefined) {
			await this.client.respond(id, "tool.renderHtml" as Method, {});
			return;
		}

		const renderer = phase === "call" ? def.renderCall : phase === "result" ? def.renderResult : undefined;
		if (renderer === undefined) {
			await this.client.respond(id, "tool.renderHtml" as Method, {});
			return;
		}

		const context: Parameters<NonNullable<ToolDefinition["renderCall"]>>[2] = {
			args: phase === "call" && isRecord(payload) ? payload : {},
			toolCallId: `html-export:${name}`,
			invalidate: () => {},
			lastComponent: undefined,
			state: {},
			cwd: this.loadOptions?.cwd ?? process.cwd(),
			executionStarted: phase === "result",
			argsComplete: true,
			isPartial: false,
			expanded: true,
			showImages: false,
			isError: false,
		};

		let component: ReturnType<NonNullable<ToolDefinition["renderCall"]>> | undefined;
		try {
			component = phase === "call"
				? def.renderCall?.(payload as never, TOOL_RENDER_THEME, context)
				: def.renderResult?.(
					payload as never,
					{ expanded: true, isPartial: false },
					TOOL_RENDER_THEME,
					context,
				);
			if (component === undefined) {
				await this.client.respond(id, "tool.renderHtml" as Method, {});
				return;
			}
			const html = ansiToInertHtml(component.render(TOOL_RENDER_WIDTH));
			await this.client.respond(id, "tool.renderHtml" as Method, { html });
		} catch (err) {
			await this.client.respondError(id, "tool.renderHtml" as Method, {
				code: "extension_error",
				message: err instanceof Error ? err.message : String(err),
				retryable: false,
			});
		} finally {
			component?.dispose?.();
		}
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
			const prepared = p["prepared"] === true
				? (args as ToolDefinition["parameters"])
				: def.prepareArguments !== undefined
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
			const cancelled = controller.signal.aborted || isStructuredAbortError(err);
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
			const stream = config.streamSimple(p["model"] as Model<string>, p["context"] as Context, options as SimpleStreamOptions);
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
			const cancelled = controller.signal.aborted || isStructuredAbortError(err);
			await this.client.respondError(id, "provider.stream" as Method, {
				code: cancelled ? "cancelled" : "extension_error",
				message: cancelled ? "provider stream cancelled" : message,
				retryable: false,
			});
		} finally {
			this.inFlightProviders.delete(id);
		}
	}

	/**
	 * Real `ExtensionActions` bridged to Rust.
	 *
	 * Bridge design (mirrors upstream `runner.bindCore` in agent-session.ts):
	 * - Void setters (`sendMessage`, `sendUserMessage`, `appendEntry`,
	 *   `setSessionName`, `setLabel`, `setActiveTools`, `refreshTools`,
	 *   `setThinkingLevel`) are fire-and-forget `session.command` events —
	 *   upstream returns `void`, so no response is observable.
	 * - Sync getters (`getSessionName`, `getActiveTools`, `getAllTools`,
	 *   `getCommands`, `getThinkingLevel`) serve the mirrored state pushed by
	 *   Rust via `session.update` (a blocking stdio round-trip cannot satisfy
	 *   a synchronous signature on the JS event loop). Rust pushes an awaited
	 *   initial snapshot before `session_start`, after every applied command,
	 *   and on relevant session events. Setters with a paired getter apply an
	 *   optimistic local mirror update BEFORE sending, so setter-then-getter
	 *   within one handler is coherent; the next authoritative push corrects
	 *   any divergence (e.g. a clamped thinking level or dropped tool name).
	 * - `setModel` is the one async member: a correlated `session.setModel`
	 *   request whose typed response carries upstream's boolean result.
	 *
	 * Documented escape hatch: the mirror carries structural equivalents of
	 * reference types (`Model`, `ToolInfo.sourceInfo`) that the host cannot
	 * instantiate as the reference classes; the final cast bridges that gap.
	 */
	private createExtensionActions(): ExtensionActions {
		const self = this;
		return {
			sendMessage: (message: Record<string, unknown>, options?: Record<string, unknown>) => {
				void self.sendSessionCommand({
					action: "sendMessage",
					message: {
						customType: message["customType"],
						content: message["content"],
						display: message["display"],
						details: message["details"],
					},
					options,
				}).catch(() => void 0);
			},
			sendUserMessage: (content: unknown, options?: Record<string, unknown>) => {
				void self.sendSessionCommand({ action: "sendUserMessage", content, options }).catch(() => void 0);
			},
			appendEntry: (customType: string, data?: unknown) => {
				void self.sendSessionCommand({ action: "appendEntry", customType, data }).catch(() => void 0);
			},
			setSessionName: (name: string) => {
				self.sessionState.sessionName = name;
				void self.sendSessionCommand({ action: "setSessionName", name }).catch(() => void 0);
			},
			getSessionName: () => self.sessionState.sessionName,
			setLabel: (entryId: string, label: string | undefined) => {
				void self.sendSessionCommand({ action: "setLabel", entryId, label }).catch(() => void 0);
			},
			getActiveTools: () => [...self.sessionState.activeTools],
			getAllTools: () =>
				self.sessionState.allTools.map((tool) => ({
					name: tool.name,
					description: tool.description,
					parameters: tool.parameters,
					sourceInfo: nativeSourceInfo(tool.source ?? "native"),
				})),
			setActiveTools: (toolNames: string[]) => {
				self.sessionState.activeTools = [...toolNames];
				void self.sendSessionCommand({ action: "setActiveTools", toolNames }).catch(() => void 0);
			},
			refreshTools: () => {
				void self.sendSessionCommand({ action: "refreshTools" }).catch(() => void 0);
			},
			getCommands: () =>
				self.sessionState.commands.map((command) => ({
					name: command.name,
					description: command.description,
					source: command.source,
					sourceInfo: nativeSourceInfo(command.source),
				})),
			setModel: async (model: unknown) => {
				try {
					const frame = await self.client.request(
						"session.setModel" as Method,
						{ model },
						{ timeoutMs: EXTENSION_HOOK_TIMEOUT_MS },
					);
					const ok = (frame.payload as Record<string, unknown>)["success"] === true;
					// Same-handler reads of ctx.model must observe the switch
					// before the follow-up session.update lands.
					if (ok) self.sessionState.model = model as typeof self.sessionState.model;
					return ok;
				} catch {
					return false;
				}
			},
			getThinkingLevel: () => self.sessionState.thinkingLevel,
			setThinkingLevel: (level: string) => {
				self.sessionState.thinkingLevel = level;
				void self.sendSessionCommand({ action: "setThinkingLevel", level }).catch(() => void 0);
			},
		} as unknown as ExtensionActions;
	}

	/**
	 * Real `ExtensionContextActions` bridged to Rust.
	 *
	 * Bridge design (verified against upstream `runner.bindCore` context
	 * actions):
	 * - Sync getters (`getModel`, `isIdle`, `hasPendingMessages`,
	 *   `getContextUsage`, `getSystemPrompt`) serve the `session.update`
	 *   mirror (see `createExtensionActions` for freshness guarantees).
	 * - `isProjectTrusted` is host-local truth (set at `extensions.load`).
	 * - `getSignal` returns the per-turn AbortController's signal; a fresh
	 *   controller is armed on every idle→busy mirror transition. `abort()`
	 *   aborts it locally (immediate, like upstream `agent.signal`) AND
	 *   forwards the abort to Rust.
	 * - `shutdown` is a fire-and-forget command. `compact` is a correlated
	 *   `session.compact` request (no timeout); `onComplete`/`onError`
	 *   receive the serialized result or error from the response frame.
	 *
	 * Documented escape hatch: `getModel` returns the mirrored plain object,
	 * not a reference `Model` instance; the final cast bridges that gap.
	 */
	private createContextActions(): ExtensionContextActions {
		const self = this;
		return {
			getModel: () => self.sessionState.model,
			getScopedModels: () => self.sessionState.scopedModels ?? [],
			isIdle: () => self.sessionState.isIdle,
			isProjectTrusted: () => self.projectTrusted,
			getSignal: () => self.turnAbort?.signal,
			abort: () => {
				self.turnAbort?.abort();
				void self.sendSessionCommand({ action: "abort" }).catch(() => void 0);
			},
			hasPendingMessages: () => self.sessionState.hasPendingMessages,
			shutdown: () => {
				void self.sendSessionCommand({ action: "shutdown" }).catch(() => void 0);
			},
			getContextUsage: () => self.sessionState.contextUsage,
			compact: (options?: {
				customInstructions?: string;
				onComplete?: (result: unknown) => void;
				onError?: (error: Error) => void;
			}) => {
				// Correlated request (no timeout: compaction can be slow);
				// upstream compact() is void with async completion callbacks.
				self.client.request(
					"session.compact" as Method,
					{ customInstructions: options?.customInstructions },
				).then(
					(frame) => {
						const result = (frame.payload as Record<string, unknown>)["result"];
						options?.onComplete?.(result);
					},
					(error: unknown) => {
						options?.onError?.(error instanceof Error ? error : new Error(String(error)));
					},
				);
			},
			getSystemPrompt: () => self.sessionState.systemPrompt,
		} as unknown as ExtensionContextActions;
	}

	/**
	 * `ExtensionCommandContextActions` for interactive command handlers.
	 *
	 * Mirrors reference `runner.bindCommandContext` wiring in interactive /
	 * print / rpc modes: `waitForIdle` is host-local (resolves on the next
	 * idle `session.update`), while `newSession` / `fork` / `navigateTree` /
	 * `switchSession` / `reload` are correlated bridge requests (same pattern
	 * as `session.setModel` / `session.compact`). Non-serializable callbacks
	 * (`setup`, `withSession`) stay host-side and run after a non-cancelled
	 * replacement: `setup` (newSession only) receives the replacement
	 * SessionManager proxy; `withSession` receives a real
	 * `ReplacedSessionContext` built from `createCommandContext()` with
	 * working `sendMessage` / `sendUserMessage` bridged to Rust.
	 */
	private createCommandContextActions(
		ownerRunner: ExtensionRunner,
		markStale?: () => void,
		assertFresh?: () => void,
	): ExtensionCommandContextActions {
		const self = this;
		/**
		 * Per-command staleness guard: reject when this command context was marked
		 * stale at token capture, or when the active runner has been replaced
		 * since bind. Lives inside each action closure so methods captured before
		 * token capture still recheck on call — without a whole-runner
		 * invalidate() or generation registry.
		 */
		const guardActive = (): void => {
			if (assertFresh !== undefined) {
				assertFresh();
				return;
			}
			if (self.runner !== ownerRunner) {
				throw new Error(STALE_COMMAND_CONTEXT_MESSAGE);
			}
		};
		const cancelledOf = (frame: Frame): boolean =>
			(frame.payload as Record<string, unknown>)["cancelled"] === true;

		const createReplacedSessionContext = (runner: ExtensionRunner): ReplacedSessionContext => {
			const context = self.createCommandContext(runner) as ReplacedSessionContext;
			context.sendMessage = async (message, options) => {
				await self.sendSessionCommand({
					action: "sendMessage",
					message: {
						customType: message.customType,
						content: message.content,
						display: message.display,
						details: message.details,
					},
					options,
				});
			};
			context.sendUserMessage = async (content, options) => {
				await self.sendSessionCommand({ action: "sendUserMessage", content, options });
			};
			return context;
		};

		const afterReplacement = async (
			cancelled: boolean,
			withSession?: (ctx: ReplacedSessionContext) => Promise<void>,
		): Promise<{ cancelled: boolean }> => {
			if (!cancelled && withSession !== undefined && self.runner !== undefined) {
				await withSession(createReplacedSessionContext(self.runner));
			}
			return { cancelled };
		};

		const captureReplacementToken = (
			payload: Record<string, unknown>,
			cancelled: boolean,
		): void => {
			if (cancelled) return;
			// Staleness follows from "the session was replaced", not from "a
			// token came back". Mark stale for every non-cancelled replacement
			// response before any token-shaped early return so that
			// createCommandContext guards reject a context bound to a session
			// that no longer exists.
			markStale?.();
			const token = payload["replacementToken"];
			if (typeof token !== "string") return;
			// Authors must await replacement calls so the token stays scoped to
			// the initiating command.execute. Fire-and-forget loses the scope
			// (or finds it already flushed); without a diagnostic the user only
			// learns via the Rust-side replacement-ready timeout, so surface the
			// drop on the spot.
			const scope = self.commandScope.getStore();
			if (scope === undefined || scope.closed) {
				self.emitExtensionError(
					"<host>",
					"session.replacementReady",
					"replacement token dropped: the replacement call was not awaited inside its command.execute handler",
				);
			} else {
				scope.tokens.push(token);
			}
		};

		return {
			waitForIdle: () => {
				guardActive();
				if (self.sessionState.isIdle) return Promise.resolve();
				const promise = new Promise<void>((resolve, reject) => {
					self.idleWaiters.push({ resolve, reject });
				});
				// Prevent unhandled rejection for fire-and-forget callers; an
				// awaiting caller still receives the rejection from dispose().
				promise.catch(() => {});
				return promise;
			},
			newSession: async (options) => {
				guardActive();
				const frame = await self.client.request(
					"session.newSession",
					{ parentSession: options?.parentSession },
					{ timeoutMs: EXTENSION_HOOK_TIMEOUT_MS },
				);
				const payload = frame.payload as Record<string, unknown>;
				const cancelled = cancelledOf(frame);
				captureReplacementToken(payload, cancelled);
				if (!cancelled && options?.setup !== undefined) {
					await options.setup(self.createSessionManagerProxy());
				}
				return afterReplacement(cancelled, options?.withSession);
			},
			fork: async (entryId, options) => {
				guardActive();
				const frame = await self.client.request(
					"session.fork",
					{ entryId, position: options?.position },
					{ timeoutMs: EXTENSION_HOOK_TIMEOUT_MS },
				);
				const payload = frame.payload as Record<string, unknown>;
				const cancelled = cancelledOf(frame);
				captureReplacementToken(payload, cancelled);
				const result = await afterReplacement(cancelled, options?.withSession);
				return {
					cancelled: result.cancelled,
					selectedText: payload["selectedText"] as string | undefined,
				};
			},
			navigateTree: async (targetId, options) => {
				guardActive();
				const summarize = options?.summarize === true;
				const frame = await self.client.request(
					"session.navigateTree",
					{
						targetId,
						summarize: options?.summarize,
						customInstructions: options?.customInstructions,
						replaceInstructions: options?.replaceInstructions,
						label: options?.label,
					},
					// Summarized navigation delegates to a provider-backed branch
					// summary that can legitimately exceed the 30 s hook deadline.
					// Only non-summarizing navigation stays under the generic timeout.
					summarize ? {} : { timeoutMs: EXTENSION_HOOK_TIMEOUT_MS },
				);
				const payload = frame.payload as Record<string, unknown>;
				return {
					cancelled: cancelledOf(frame),
					editorText: payload["editorText"] as string | undefined,
					aborted: payload["aborted"] as boolean | undefined,
					summaryEntry: payload["summaryEntry"] as BranchSummaryEntry | undefined,
				};
			},
			switchSession: async (sessionPath, options) => {
				guardActive();
				const frame = await self.client.request(
					"session.switchSession",
					{ sessionPath },
					{ timeoutMs: EXTENSION_HOOK_TIMEOUT_MS },
				);
				const payload = frame.payload as Record<string, unknown>;
				const cancelled = cancelledOf(frame);
				captureReplacementToken(payload, cancelled);
				return afterReplacement(cancelled, options?.withSession);
			},
			reload: async () => {
				guardActive();
				const frame = await self.client.request(
					"session.reload",
					{},
					{ timeoutMs: EXTENSION_HOOK_TIMEOUT_MS },
				);
				const payload = frame.payload as Record<string, unknown>;
				// Reload has no cancelled flag; capture any token and strip from extension view.
				captureReplacementToken(payload, false);
			},
		};
	}

	private createCommandContext(runner: ExtensionRunner): ExtensionCommandContext {
		let stale = false;
		const guard = (): void => {
			if (stale || this.runner !== runner) {
				throw new Error(STALE_COMMAND_CONTEXT_MESSAGE);
			}
		};
		// Pass guard into action closures so pre-captured methods recheck stale.
		const actions = this.createCommandContextActions(runner, () => {
			stale = true;
		}, guard);

		return new Proxy(runner.createCommandContext(), {
			get(target, property, receiver) {
				guard();
				switch (property) {
					case "waitForIdle":
						return actions.waitForIdle;
					case "newSession":
						return actions.newSession;
					case "fork":
						return actions.fork;
					case "navigateTree":
						return actions.navigateTree;
					case "switchSession":
						return actions.switchSession;
					case "reload":
						return actions.reload;
					default:
						return Reflect.get(target, property, receiver);
				}
			},
		});
	}

	// Honest narrow bridge: SessionManager is a 1600-line reference class the
	// host cannot instantiate. Rust owns the real session tree. Only matching
	// SessionManager mutations route through `session.command`; each returns
	// the write-delivery Promise rather than a fabricated synchronous entry ID.
	// The uncorrelated wire therefore cannot support ID-dependent chaining.
	// Its one mirrored getter is `getSessionName`. Every other SessionManager
	// method fails explicitly instead of silently no-op-ing.
	private createSessionManagerProxy(): SessionManagerSetupBridge {
		const self = this;
		const unsupported = (method: string) => () => {
			throw new Error(
				`SessionManager method '${method}' is not supported via the extension bridge`,
			);
		};
		return new Proxy({}, {
			get(_target, prop) {
				if (typeof prop !== "string") return undefined;
				// Never expose a callable `then`: the proxy must not look like a thenable.
				if (prop === "then") return undefined;
				// Logging/printing/probing the bridge must not throw: forward
				// `Object.prototype` members (`toString`, `valueOf`,
				// `hasOwnProperty`, `constructor`, …) and leave `toJSON`
				// undefined so `JSON.stringify` serializes instead of throwing.
				// Only real SessionManager methods fail loudly.
				if (prop === "toJSON") return undefined;
				const inherited = Reflect.get(Object.prototype, prop);
				if (inherited !== undefined) return inherited;
				switch (prop) {
					case "appendCustomEntry":
						return (customType: string, data?: unknown) =>
							self.sendSessionCommand({ action: "appendEntry", customType, data });
					case "appendSessionInfo":
						return async (name: string) => {
							await self.sendSessionCommand({ action: "setSessionName", name });
							self.sessionState.sessionName = name;
						};
					case "getSessionName":
						return () => self.sessionState.sessionName;
					default:
						return unsupported(prop);
				}
			},
		}) as SessionManagerSetupBridge;
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

	private terminate(reason: string): void {
		this.assistantDelta.clearActiveAssistant();
		console.error(`[host] fatal: ${reason}`);
		this.dispose(reason);
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
		for (const controller of this.inFlightShortcuts.values()) controller.abort();
		this.inFlightShortcuts.clear();
		this.providers.clear();
		// Settle pending waitForIdle waiters: idle was never reached, so
		// reject with the disposal reason. Each waiter has a no-op catch
		// attached at creation, so this cannot become an unhandled rejection.
		const waiters = this.idleWaiters.splice(0);
		for (const waiter of waiters) waiter.reject(new Error(reason));
		for (const key of [...this.slots.keys()]) this.disposeSlot(key);
		this.client.dispose(reason);
	}

	get isDisposed(): boolean { return this.state === HostState.DISPOSED; }
	get extensionCount(): number { return this.extensions.length; }
	getExtensions(): Extension[] { return [...this.extensions]; }
	getRunner(): ExtensionRunner | undefined { return this.runner; }
}

function isRecord<T extends Record<string, unknown>>(value: unknown): value is T {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Compare JSON-serializable values ignoring object-key insertion order. */
function canonicalJsonEqual(left: unknown, right: unknown): boolean {
	return JSON.stringify(canonicalizeJson(left)) === JSON.stringify(canonicalizeJson(right));
}

function canonicalizeJson(value: unknown): unknown {
	if (Array.isArray(value)) return value.map(canonicalizeJson);
	if (isRecord(value)) {
		const sorted = Object.create(null) as Record<string, unknown>;
		for (const key of Object.keys(value).sort()) {
			sorted[key] = canonicalizeJson(value[key]);
		}
		return sorted;
	}
	return value;
}
