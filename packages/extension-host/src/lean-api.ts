/**
 * Lean extension API (Mode 2).
 *
 * A lean extension is a prebundled `.mjs` module whose default export is a
 * declarative {@link LeanExtension} — conventionally authored with
 * {@link defineExtension}. The lean runner (`lean-runner.ts`) imports the
 * entry with a plain dynamic `import()`: no jiti, no upstream coding-agent
 * runtime graph. The exported surface is validated structurally at load
 * time; anything unknown or unsupported is a per-extension load error,
 * never a host failure.
 *
 * This module is self-contained on purpose: it MUST NOT import from the
 * upstream `@earendil-works/*` packages so prebundled entries and the lean
 * runner share zero runtime graph with Mode 1.
 */

/** Lifecycle event discriminants (mirrors Rust `ALL_EVENT_TYPES`). */
export const LEAN_EVENT_TYPES = [
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

/** One lifecycle event discriminant. */
export type LeanEventType = (typeof LEAN_EVENT_TYPES)[number];

const LEAN_EVENT_TYPE_SET: ReadonlySet<string> = new Set(LEAN_EVENT_TYPES);

/** Returns true when `raw` is a known lifecycle event discriminant. */
export function isLeanEventType(raw: string): raw is LeanEventType {
	return LEAN_EVENT_TYPE_SET.has(raw);
}

/** Minimal per-extension context handed to every lean callback. */
export interface LeanContext {
	/** Working directory the host was loaded with. */
	readonly cwd: string;
	/** Entry path of the extension that registered this callback. */
	readonly extensionPath: string;
}

/** Context for a `tool.execute` invocation. */
export interface LeanToolContext extends LeanContext {
	/** Correlation id of the tool call. */
	readonly toolCallId: string;
	/** Aborted by `tool.cancel`; honor it for cooperative cancellation. */
	readonly signal: AbortSignal;
	/** Emit a streaming partial result (`toolUpdate` event). */
	readonly onUpdate: (partialResult: unknown) => void;
}

/**
 * Declarative tool. `prepare`/`validate` stay REAL RPCs: the agent loop
 * round-trips through the host even when a tool omits them (the runner then
 * echoes the arguments, mirroring Mode 1 defaults).
 */
export interface LeanTool {
	readonly name: string;
	readonly label?: string;
	readonly description: string;
	/** JSON Schema for the arguments; forwarded to the model. */
	readonly parameters?: Record<string, unknown>;
	readonly executionMode?: string;
	/** Map raw model arguments before validation. Defaults to identity. */
	readonly prepare?: (args: unknown, ctx: LeanContext) => unknown | Promise<unknown>;
	/** Validate (and optionally normalize) prepared args; throw to reject. */
	readonly validate?: (args: unknown, ctx: LeanContext) => unknown | Promise<unknown>;
	/** Run the tool; the return value crosses the wire as the tool result. */
	readonly execute: (args: unknown, ctx: LeanToolContext) => unknown | Promise<unknown>;
}

/** Declarative slash command. */
export interface LeanCommand {
	readonly name: string;
	readonly description?: string;
	readonly handler: (args: string, ctx: LeanContext) => void | Promise<void>;
}

/** Declarative CLI flag. */
export interface LeanFlag {
	readonly name: string;
	readonly description?: string;
	readonly type: "boolean" | "string";
	readonly default?: boolean | string;
}

/** Declarative keyboard shortcut. */
export interface LeanShortcut {
	readonly key: string;
	readonly description?: string;
	readonly handler: (ctx: LeanContext) => void | Promise<void>;
}

/** Declarative custom provider (mirrors the Mode 1 provider wire shape). */
export interface LeanProvider {
	readonly name: string;
	readonly displayName?: string;
	readonly baseUrl?: string;
	readonly api?: string;
	readonly apiKey?: string;
	readonly headers?: Record<string, string>;
	readonly authHeader?: boolean;
	readonly models?: readonly unknown[];
	/** Stream one assistant turn; events cross the wire as `providerEvent`. */
	readonly streamSimple?: (
		model: unknown,
		context: unknown,
		options: Record<string, unknown> & { signal: AbortSignal },
	) => AsyncIterable<unknown>;
}

// ---------------------------------------------------------------------------
// Typed lifecycle hooks
// ---------------------------------------------------------------------------

export interface LeanToolCallEvent {
	readonly type: "tool_call";
	readonly toolName: string;
	readonly toolCallId: string;
	/** Mutable: edits are threaded back to the agent loop. */
	readonly input: Record<string, unknown>;
}

export interface LeanToolCallHookResult {
	readonly block?: boolean;
	readonly reason?: string;
}

export interface LeanToolResultEvent {
	readonly type: "tool_result";
	readonly toolName: string;
	readonly toolCallId: string;
	readonly input: unknown;
	readonly content: unknown;
	readonly details: unknown;
	readonly isError: boolean;
}

export interface LeanToolResultHookResult {
	readonly content?: unknown;
	readonly details?: unknown;
	readonly isError?: boolean;
}

export interface LeanMessageEndEvent {
	readonly type: "message_end";
	readonly message: Record<string, unknown>;
	readonly [key: string]: unknown;
}

export interface LeanMessageEndHookResult {
	/** Replacement message; the role MUST match the original. */
	readonly message?: Record<string, unknown>;
}

export interface LeanInputEvent {
	readonly type: "input";
	readonly text: string;
	readonly images?: unknown;
	readonly source: string;
	readonly streamingBehavior?: string;
}

export type LeanInputHookResult =
	| { readonly action: "handled" }
	| { readonly action: "transform"; readonly text: string; readonly images?: unknown }
	| { readonly action: "continue" };

export interface LeanBeforeAgentStartEvent {
	readonly type: "before_agent_start";
	readonly prompt: string;
	readonly images?: unknown;
	readonly systemPrompt: string;
	readonly systemPromptOptions: { readonly cwd: string };
}

export interface LeanBeforeAgentStartHookResult {
	readonly message?: Record<string, unknown>;
	readonly systemPrompt?: string;
}

export interface LeanResourcesDiscoverEvent {
	readonly type: "resources_discover";
	readonly cwd: string;
	readonly reason: string;
}

export interface LeanResourcesDiscoverHookResult {
	readonly skillPaths?: readonly string[];
	readonly promptPaths?: readonly string[];
	readonly themePaths?: readonly string[];
}

/** Generic hook event for every lifecycle type without a shaped payload. */
export interface LeanGenericEvent {
	readonly type: string;
	readonly [key: string]: unknown;
}

/** Generic hook result; `{cancel: true, reason?}` short-circuits where supported. */
export interface LeanGenericHookResult {
	readonly cancel?: boolean;
	readonly reason?: string;
	readonly [key: string]: unknown;
}

/** Typed hooks for the shaped lifecycle events. */
export interface LeanShapedHooks {
	readonly tool_call?: (
		event: LeanToolCallEvent,
		ctx: LeanContext,
	) => LeanToolCallHookResult | void | Promise<LeanToolCallHookResult | void>;
	readonly tool_result?: (
		event: LeanToolResultEvent,
		ctx: LeanContext,
	) => LeanToolResultHookResult | void | Promise<LeanToolResultHookResult | void>;
	readonly message_end?: (
		event: LeanMessageEndEvent,
		ctx: LeanContext,
	) => LeanMessageEndHookResult | void | Promise<LeanMessageEndHookResult | void>;
	readonly input?: (
		event: LeanInputEvent,
		ctx: LeanContext,
	) => LeanInputHookResult | void | Promise<LeanInputHookResult | void>;
	readonly before_agent_start?: (
		event: LeanBeforeAgentStartEvent,
		ctx: LeanContext,
	) => LeanBeforeAgentStartHookResult | void | Promise<LeanBeforeAgentStartHookResult | void>;
	readonly resources_discover?: (
		event: LeanResourcesDiscoverEvent,
		ctx: LeanContext,
	) => LeanResourcesDiscoverHookResult | void | Promise<LeanResourcesDiscoverHookResult | void>;
}

/** Generic hook signature for the remaining lifecycle events. */
export type LeanGenericHook = (
	event: LeanGenericEvent,
	ctx: LeanContext,
) => unknown | Promise<unknown>;

/** Lifecycle hooks keyed by event discriminant. */
export type LeanHooks = LeanShapedHooks &
	Partial<Record<Exclude<LeanEventType, keyof LeanShapedHooks>, LeanGenericHook>>;

// ---------------------------------------------------------------------------
// Extension definition
// ---------------------------------------------------------------------------

/** Declarative lean extension definition (the `.mjs` default export). */
export interface LeanExtension {
	readonly name?: string;
	readonly tools?: readonly LeanTool[];
	readonly commands?: readonly LeanCommand[];
	readonly flags?: readonly LeanFlag[];
	readonly shortcuts?: readonly LeanShortcut[];
	readonly providers?: readonly LeanProvider[];
	readonly hooks?: LeanHooks;
}

const DEFINITION_KEYS: ReadonlySet<string> = new Set([
	"name",
	"tools",
	"commands",
	"flags",
	"shortcuts",
	"providers",
	"hooks",
]);

const TOOL_KEYS: ReadonlySet<string> = new Set([
	"name",
	"label",
	"description",
	"parameters",
	"executionMode",
	"prepare",
	"validate",
	"execute",
]);
const COMMAND_KEYS: ReadonlySet<string> = new Set(["name", "description", "handler"]);
const FLAG_KEYS: ReadonlySet<string> = new Set(["name", "description", "type", "default"]);
const SHORTCUT_KEYS: ReadonlySet<string> = new Set(["key", "description", "handler"]);
const PROVIDER_KEYS: ReadonlySet<string> = new Set([
	"name",
	"displayName",
	"baseUrl",
	"api",
	"apiKey",
	"headers",
	"authHeader",
	"models",
	"streamSimple",
]);

function requireKnownKeys(
	context: string,
	value: Record<string, unknown>,
	allowed: ReadonlySet<string>,
): void {
	for (const key of Object.keys(value)) {
		if (!allowed.has(key)) {
			fail(context, `unknown key "${key}"`);
		}
	}
}

/**
 * Identity helper giving lean extensions a typed authoring surface. The
 * runner validates structurally, so prebundled entries may also export a
 * plain object literal of the same shape.
 */
export function defineExtension(extension: LeanExtension): LeanExtension {
	return extension;
}

/** Raised by {@link parseLeanExtension} for unknown/unsupported surfaces. */
export class LeanSurfaceError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "LeanSurfaceError";
	}
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function fail(context: string, problem: string): never {
	throw new LeanSurfaceError(`${context}: ${problem}`);
}

function requireString(context: string, value: unknown, field: string): void {
	if (typeof value !== "string" || value === "") {
		fail(context, `${field} must be a non-empty string`);
	}
}

function requireFunction(context: string, value: unknown, field: string): void {
	if (typeof value !== "function") {
		fail(context, `${field} must be a function`);
	}
}

function optionalString(context: string, value: unknown, field: string): void {
	if (value !== undefined && typeof value !== "string") {
		fail(context, `${field} must be a string when present`);
	}
}

function parseTools(value: unknown): void {
	if (value === undefined) return;
	if (!Array.isArray(value)) fail("tools", "must be an array");
	for (const [index, tool] of value.entries()) {
		const context = `tools[${index}]`;
		if (!isRecord(tool)) fail(context, "must be an object");
		requireKnownKeys(context, tool, TOOL_KEYS);
		requireString(context, tool["name"], "name");
		requireString(context, tool["description"], "description");
		optionalString(context, tool["label"], "label");
		optionalString(context, tool["executionMode"], "executionMode");
		if (tool["parameters"] !== undefined && !isRecord(tool["parameters"])) {
			fail(context, "parameters must be a JSON Schema object");
		}
		if (tool["prepare"] !== undefined) requireFunction(context, tool["prepare"], "prepare");
		if (tool["validate"] !== undefined) requireFunction(context, tool["validate"], "validate");
		requireFunction(context, tool["execute"], "execute");
	}
}

function parseCommands(value: unknown): void {
	if (value === undefined) return;
	if (!Array.isArray(value)) fail("commands", "must be an array");
	for (const [index, command] of value.entries()) {
		const context = `commands[${index}]`;
		if (!isRecord(command)) fail(context, "must be an object");
		requireKnownKeys(context, command, COMMAND_KEYS);
		requireString(context, command["name"], "name");
		optionalString(context, command["description"], "description");
		requireFunction(context, command["handler"], "handler");
	}
}

function parseFlags(value: unknown): void {
	if (value === undefined) return;
	if (!Array.isArray(value)) fail("flags", "must be an array");
	for (const [index, flag] of value.entries()) {
		const context = `flags[${index}]`;
		if (!isRecord(flag)) fail(context, "must be an object");
		requireKnownKeys(context, flag, FLAG_KEYS);
		requireString(context, flag["name"], "name");
		optionalString(context, flag["description"], "description");
		if (flag["type"] !== "boolean" && flag["type"] !== "string") {
			fail(context, 'type must be "boolean" or "string"');
		}
		const defaultValue = flag["default"];
		if (defaultValue !== undefined) {
			const expected = flag["type"] === "boolean" ? "boolean" : "string";
			if (typeof defaultValue !== expected) {
				fail(context, `default must be a ${expected} for type "${flag["type"]}"`);
			}
		}
	}
}

function parseShortcuts(value: unknown): void {
	if (value === undefined) return;
	if (!Array.isArray(value)) fail("shortcuts", "must be an array");
	for (const [index, shortcut] of value.entries()) {
		const context = `shortcuts[${index}]`;
		if (!isRecord(shortcut)) fail(context, "must be an object");
		requireKnownKeys(context, shortcut, SHORTCUT_KEYS);
		requireString(context, shortcut["key"], "key");
		optionalString(context, shortcut["description"], "description");
		requireFunction(context, shortcut["handler"], "handler");
	}
}

function parseProviders(value: unknown): void {
	if (value === undefined) return;
	if (!Array.isArray(value)) fail("providers", "must be an array");
	for (const [index, provider] of value.entries()) {
		const context = `providers[${index}]`;
		if (!isRecord(provider)) fail(context, "must be an object");
		requireKnownKeys(context, provider, PROVIDER_KEYS);
		requireString(context, provider["name"], "name");
		optionalString(context, provider["displayName"], "displayName");
		optionalString(context, provider["baseUrl"], "baseUrl");
		optionalString(context, provider["api"], "api");
		optionalString(context, provider["apiKey"], "apiKey");
		if (provider["authHeader"] !== undefined && typeof provider["authHeader"] !== "boolean") {
			fail(context, "authHeader must be a boolean when present");
		}
		if (provider["headers"] !== undefined) {
			if (!isRecord(provider["headers"])) fail(context, "headers must be an object");
			for (const [key, headerValue] of Object.entries(provider["headers"] as Record<string, unknown>)) {
				if (typeof headerValue !== "string") {
					fail(context, `headers.${key} must be a string`);
				}
			}
		}
		if (provider["models"] !== undefined && !Array.isArray(provider["models"])) {
			fail(context, "models must be an array when present");
		}
		if (provider["streamSimple"] !== undefined) {
			requireFunction(context, provider["streamSimple"], "streamSimple");
		}
	}
}

function parseHooks(value: unknown): void {
	if (value === undefined) return;
	if (!isRecord(value)) fail("hooks", "must be an object");
	for (const [eventType, handler] of Object.entries(value)) {
		if (!isLeanEventType(eventType)) {
			fail("hooks", `unknown lifecycle event "${eventType}"`);
		}
		if (handler !== undefined) requireFunction(`hooks.${eventType}`, handler, "handler");
	}
}

/**
 * Structurally validate an imported module's default export. Returns the
 * definition unchanged on success; throws {@link LeanSurfaceError} with a
 * precise path when the surface is unknown or unsupported. The caller turns
 * that into a per-extension load error.
 */
export function parseLeanExtension(value: unknown): LeanExtension {
	if (typeof value === "function") {
		fail(
			"default export",
			"must be a declarative lean extension object, got a function " +
				"(compat factories require the Mode 1 host)",
		);
	}
	if (!isRecord(value)) {
		fail("default export", "must be a declarative lean extension object");
	}
	requireKnownKeys("default export", value, DEFINITION_KEYS);
	if (value["name"] !== undefined) {
		requireString("default export", value["name"], "name");
	}
	parseTools(value["tools"]);
	parseCommands(value["commands"]);
	parseFlags(value["flags"]);
	parseShortcuts(value["shortcuts"]);
	parseProviders(value["providers"]);
	parseHooks(value["hooks"]);
	return value as LeanExtension;
}
