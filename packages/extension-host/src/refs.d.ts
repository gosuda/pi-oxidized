/**
 * Narrow typed declarations for reference pi packages.
 *
 * Isolates the compiler from reference `.ts` source (which has pre-existing
 * type errors in proxy.ts and syntax-highlight.ts). At runtime, Bun resolves
 * the real source through relative `tsconfig` path mappings; tests validate
 * the bridge.
 */

declare module "@earendil-works/pi-coding-agent" {
	/** Bridge-local TUI surface mirroring the observable shape extensions use. */
	export interface TuiBridge {
		requestRender(): void;
	}
	import type {
		AssistantMessageEventStream,
		Context,
		ImageContent,
		Model,
		SimpleStreamOptions,
		TextContent,
	} from "@earendil-works/pi-ai";
	export type ExtensionMode = "tui" | "rpc" | "json" | "print";

	export type SourceScope = "user" | "project" | "temporary";
	export type SourceOrigin = "package" | "top-level";

	export interface SourceInfo {
		path: string;
		source: string;
		scope: SourceScope;
		origin: SourceOrigin;
		baseDir?: string;
	}

	export interface ExtensionUIContext {
		select(title: string, options: string[], opts?: { timeout?: number }): Promise<string | undefined>;
		confirm(title: string, message: string, opts?: { timeout?: number }): Promise<boolean>;
		input(title: string, placeholder?: string, opts?: { timeout?: number }): Promise<string | undefined>;
		editor(title: string, prefill?: string): Promise<string | undefined>;
		notify(message: string, type?: string): void;
		onTerminalInput(handler: (data: string) => unknown): () => void;
		setStatus(key: string, text: string | undefined): void;
		setWorkingMessage(message?: string): void;
		setWorkingVisible(visible: boolean): void;
		setWorkingIndicator(options?: { frames?: string[]; intervalMs?: number }): void;
		setHiddenThinkingLabel(label?: string): void;
		setWidget(key: string, content: string[] | ((tui: unknown, theme: Theme) => Component) | undefined, options?: Record<string, unknown>): void;
		setFooter(factory: ((tui: unknown, theme: Theme, footerData: unknown) => Component) | undefined): void;
		setHeader(factory: ((tui: unknown, theme: Theme) => Component) | undefined): void;
		setTitle(title: string): void;
		pasteToEditor(text: string): void;
		setEditorText(text: string): void;
		getEditorText(): string;
		addAutocompleteProvider(factory: unknown): void;
		setEditorComponent(factory: unknown): void;
		getEditorComponent(): unknown;
		custom<T>(factory: (tui: TuiBridge, theme: Theme, keybindings: unknown, done: (result: T) => void) => unknown, options?: unknown): Promise<T>;
		readonly theme: Theme;
		getAllThemes(): { name: string; path: string | undefined }[];
		getTheme(name: string): Theme | undefined;
		setTheme(theme: string | Theme): { success: boolean; error?: string };
		getToolsExpanded(): boolean;
		setToolsExpanded(expanded: boolean): void;
	}

	export interface ContextUsage {
		tokens?: number;
		contextWindow: number;
		percent?: number;
	}

	export interface ExtensionContext {
		ui: ExtensionUIContext;
		mode: ExtensionMode;
		hasUI: boolean;
		cwd: string;
		readonly model: unknown;
		readonly scopedModels: readonly unknown[];
		readonly signal: AbortSignal | undefined;
		isIdle(): boolean;
		isProjectTrusted(): boolean;
		abort(): void;
		hasPendingMessages(): boolean;
		shutdown(): void;
		getContextUsage(): ContextUsage | undefined;
		compact(options?: {
			customInstructions?: string;
			onComplete?: (result: unknown) => void;
			onError?: (error: Error) => void;
		}): void;
		getSystemPrompt(): string;
	}

	/**
	 * Narrow SessionManager surface passed to `newSession({ setup })`.
	 * Mirrors `ExtensionHost.createSessionManagerProxy()` — only supported
	 * bridge methods; unsupported SessionManager APIs throw at runtime.
	 */
	export interface SessionManagerSetupBridge {
		appendCustomEntry(customType: string, data?: unknown): Promise<void>;
		appendSessionInfo(name: string): Promise<void>;
		getSessionName(): string | undefined;
		getEntries(): unknown[];
	}
	export interface BranchSummaryEntry {
		type: "branch_summary";
		id: string;
		parentId: string | null;
		timestamp: string;
		fromId: string;
		summary: string;
		details?: unknown;
	}

	/** Bridge-local skill shape mirroring the pinned pi-coding-agent type. */
	export interface Skill {
		name: string;
		description: string;
		filePath: string;
		baseDir: string;
		sourceInfo: SourceInfo;
		disableModelInvocation: boolean;
	}

	export interface ExtensionCommandContext extends ExtensionContext {
	getSystemPromptOptions(): BuildSystemPromptOptions;
		waitForIdle(): Promise<void>;
		newSession(options?: {
			parentSession?: string;
			setup?: (sessionManager: SessionManagerSetupBridge) => Promise<void>;
			withSession?: (ctx: ReplacedSessionContext) => Promise<void>;
		}): Promise<{ cancelled: boolean }>;
		fork(
			entryId: string,
			options?: { position?: "before" | "at"; withSession?: (ctx: ReplacedSessionContext) => Promise<void> },
		): Promise<{ cancelled: boolean; selectedText?: string }>;
		navigateTree(
			targetId: string,
			options?: {
				summarize?: boolean;
				customInstructions?: string;
				replaceInstructions?: boolean;
				label?: string;
			},
		): Promise<{
			cancelled: boolean;
			editorText?: string;
			aborted?: boolean;
			summaryEntry?: BranchSummaryEntry;
		}>;
		switchSession(
			sessionPath: string,
			options?: { withSession?: (ctx: ReplacedSessionContext) => Promise<void> },
		): Promise<{ cancelled: boolean }>;
		reload(): Promise<void>;
	}

	/**
	 * Fresh command-capable context bound to the replacement session after a
	 * session switch. Passed to `withSession()` callbacks on `newSession()`,
	 * `fork()`, and `switchSession()`.
	 */
	export interface ReplacedSessionContext extends ExtensionCommandContext {
		sendMessage(
			message: { customType: string; content: string | (TextContent | ImageContent)[]; display: boolean; details?: unknown },
			options?: { triggerTurn?: boolean; deliverAs?: "steer" | "followUp" | "nextTurn" },
		): Promise<void>;
		sendUserMessage(
			content: string | (TextContent | ImageContent)[],
			options?: { deliverAs?: "steer" | "followUp" },
		): Promise<void>;
	}

	export interface BuildSystemPromptOptions {
		customPrompt?: string;
		selectedTools?: string[];
		toolSnippets?: Record<string, string>;
		promptGuidelines?: string[];
		appendSystemPrompt?: string;
		cwd: string;
		contextFiles?: Array<{ path: string; content: string }>;
		skills?: Skill[];
	}

	export type ToolExecutionMode = "sequential" | "parallel";

	export interface AgentToolResult<TDetails = unknown> {
		content: Array<{ type: string; text?: string; [key: string]: unknown }>;
		details?: TDetails;
		isError?: boolean;
	}

	export interface Component {
		render(width: number): string[];
		handleInput?(data: string): void;
		dispose?(): void;
	}

	export interface Theme {
		name?: string;
		fg(color: string, text: string): string;
		bg(color: string, text: string): string;
		bold(text: string): string;
		italic(text: string): string;
		underline(text: string): string;
		inverse(text: string): string;
		strikethrough(text: string): string;
		getFgAnsi(color: string): string;
		getBgAnsi(color: string): string;
		getColorMode(): string;
		getThinkingBorderColor(level: string): (text: string) => string;
		getBashModeBorderColor(): (text: string) => string;
	}

	export interface ToolRenderContext<TState = unknown, TArgs = unknown> {
		args: TArgs;
		toolCallId: string;
		invalidate(): void;
		lastComponent: Component | undefined;
		state: TState;
		cwd: string;
		executionStarted: boolean;
		argsComplete: boolean;
		isPartial: boolean;
		expanded: boolean;
		showImages: boolean;
		isError: boolean;
	}

	export interface ToolRenderResultOptions {
		expanded: boolean;
		isPartial: boolean;
	}

	export interface ToolDefinition<TParams = Record<string, unknown>, TDetails = unknown> {
		name: string;
		label: string;
		description: string;
		parameters: TParams;
		executionMode?: ToolExecutionMode;
		prepareArguments?: (args: unknown) => TParams;
		execute(
			toolCallId: string,
			params: TParams,
			signal: AbortSignal | undefined,
			onUpdate: ((partial: AgentToolResult<TDetails>) => void) | undefined,
			ctx: ExtensionContext,
		): Promise<AgentToolResult<TDetails>>;
		renderCall?: (
			args: TParams,
			theme: Theme,
			context: ToolRenderContext<unknown, TParams>,
		) => Component;
		renderResult?: (
			result: AgentToolResult<TDetails>,
			options: ToolRenderResultOptions,
			theme: Theme,
			context: ToolRenderContext<unknown, TParams>,
		) => Component;
	}

	export function defineTool<T extends ToolDefinition>(tool: T): T;

	export interface RegisteredTool {
		definition: ToolDefinition;
		sourceInfo: SourceInfo;
	}

	export interface ResolvedCommand extends RegisteredCommand {
		invocationName: string;
	}

	export interface RegisteredCommand {
		name: string;
		sourceInfo: SourceInfo;
		description?: string;
		handler: (args: string, ctx: ExtensionCommandContext) => Promise<void>;
	}

	export interface ExtensionFlag {
		name: string;
		description?: string;
		type: "boolean" | "string";
		default?: boolean | string;
		extensionPath: string;
	}

	export interface ExtensionShortcut {
		shortcut: string;
		description?: string;
		handler: (ctx: ExtensionContext) => Promise<void> | void;
		extensionPath: string;
	}

	export interface ProviderModelConfig {
		id: string;
		name: string;
		api?: string;
		baseUrl?: string;
		reasoning: boolean;
		input: Array<"text" | "image">;
		cost: unknown;
		contextWindow: number;
		maxTokens: number;
		headers?: Record<string, string>;
	}

	export interface ProviderConfig {
		name?: string;
		baseUrl?: string;
		apiKey?: string;
		api?: string;
		streamSimple?: (
			model: Model<string>,
			context: Context,
			options?: SimpleStreamOptions,
		) => AssistantMessageEventStream;
		headers?: Record<string, string>;
		authHeader?: boolean;
		models?: ProviderModelConfig[];
	}

	export interface ExtensionError {
		extensionPath: string;
		event: string;
		error: string;
		stack?: string;
	}

	export type ExtensionErrorListener = (error: ExtensionError) => void;

	export interface SessionStartEvent { type: "session_start"; reason: string }
	export interface AgentStartEvent { type: "agent_start" }
	export interface MessageEndEvent { type: "message_end"; message: unknown }
	export interface ContextEvent { type: "context"; messages: unknown[] }
	export interface InputEvent { type: "input"; source: string }
	export interface ToolCallEvent {
		type: "tool_call";
		toolName: string;
		toolCallId: string;
		input: Record<string, unknown>;
	}
	export interface ToolResultEvent {
		type: "tool_result";
		toolName: string;
		toolCallId: string;
		input: Record<string, unknown>;
		content: unknown[];
		details?: unknown;
		isError: boolean;
		usage?: unknown;
	}
	export interface BeforeAgentStartEvent {
		type: "before_agent_start";
		prompt: string;
		images?: unknown[];
		systemPrompt: string;
		systemPromptOptions: BuildSystemPromptOptions;
	}
	export interface ToolCallEventResult {
		block?: boolean;
		reason?: string;
		terminate?: boolean;
	}
	export interface ToolResultEventResult {
		content?: unknown[];
		details?: unknown;
		isError?: boolean;
		usage?: unknown;
	}
	export interface BeforeAgentStartEventResult {
		message?: unknown;
		systemPrompt?: string;
	}
	/** Combined result from all before_agent_start handlers (runner aggregate). */
	export interface BeforeAgentStartCombinedResult {
		messages?: NonNullable<BeforeAgentStartEventResult["message"]>[];
		systemPrompt?: string;
	}
	export interface TurnStartEvent { type: "turn_start"; turnIndex: number }
	export interface ToolExecutionStartEvent { type: "tool_execution_start"; toolName: string }

	export type ExtensionHandler<E, R = void> = (
		event: E, ctx: ExtensionContext,
	) => Promise<R | void> | R | void;

	export interface Extension {
		path: string;
		resolvedPath: string;
		hidden?: boolean;
		sourceInfo: SourceInfo;
		handlers: Map<string, Array<(event: unknown, ctx: ExtensionContext) => Promise<unknown>>>;
		tools: Map<string, RegisteredTool>;
		commands: Map<string, RegisteredCommand>;
		flags: Map<string, ExtensionFlag>;
		shortcuts: Map<string, ExtensionShortcut>;
		messageRenderers: Map<string, unknown>;
		entryRenderers?: Map<string, unknown>;
	}

	export type ExtensionFactory = (pi: ExtensionAPI) => void | Promise<void>;

	export type InlineExtension =
		| ExtensionFactory
		| {
				name: string;
				factory: ExtensionFactory;
				hidden?: boolean;
		  };

	export interface ExtensionAPI {
		on(event: "session_start", handler: ExtensionHandler<SessionStartEvent>): void;
		on(event: "agent_start", handler: ExtensionHandler<AgentStartEvent>): void;
		on(event: "message_end", handler: ExtensionHandler<MessageEndEvent, { message?: unknown }>): void;
		on(event: "context", handler: ExtensionHandler<ContextEvent, { messages?: unknown[] }>): void;
		on(event: "input", handler: ExtensionHandler<InputEvent, { action: string; text?: string }>): void;
		on(event: "tool_call", handler: ExtensionHandler<ToolCallEvent, ToolCallEventResult>): void;
		on(event: "tool_result", handler: ExtensionHandler<ToolResultEvent, ToolResultEventResult>): void;
		on(event: "turn_start", handler: ExtensionHandler<TurnStartEvent>): void;
		on(event: "tool_execution_start", handler: ExtensionHandler<ToolExecutionStartEvent>): void;
		on(event: string, handler: (...args: unknown[]) => unknown): void;
		registerTool<T extends ToolDefinition>(tool: T): void;
		registerCommand(name: string, options: Omit<RegisteredCommand, "name" | "sourceInfo">): void;
		registerShortcut(shortcut: string, options: Record<string, unknown>): void;
		registerFlag(name: string, options: Record<string, unknown>): void;
		registerMessageRenderer(customType: string, renderer: unknown): void;
		registerProvider(name: string, config: ProviderConfig): void;
		unregisterProvider(name: string): void;
		getFlag(name: string): boolean | string | undefined;
		sendMessage(
			message: { customType: string; content: string | (TextContent | ImageContent)[]; display: boolean; details?: unknown },
			options?: { triggerTurn?: boolean; deliverAs?: "steer" | "followUp" | "nextTurn" },
		): void;
		sendUserMessage(content: string | (TextContent | ImageContent)[], options?: { deliverAs?: "steer" | "followUp" }): void;
		appendEntry(customType: string, data?: unknown): void;
		setSessionName(name: string): void;
		getSessionName(): string | undefined;
		setLabel(entryId: string, label: string | undefined): void;
		getActiveTools(): string[];
		getAllTools(): Array<{ name: string; description: string; parameters: unknown; sourceInfo: unknown }>;
		setActiveTools(toolNames: string[]): void;
		getCommands(): Array<{ name: string; description?: string; source: string; sourceInfo: unknown }>;
		setModel(model: unknown): Promise<boolean>;
		getThinkingLevel(): string;
		setThinkingLevel(level: string): void;
	}

	export interface ExtensionActions {
		sendMessage: (message: unknown) => void;
		sendUserMessage: (content: unknown) => void;
		appendEntry: (customType: string, data?: unknown) => void;
		setSessionName: (name: string) => void;
		getSessionName: () => string | undefined;
		setLabel: (entryId: string, label: string | undefined) => void;
		getActiveTools: () => string[];
		getAllTools: () => unknown[];
		setActiveTools: (toolNames: string[]) => void;
		refreshTools: () => void;
		getCommands: () => unknown[];
		setModel: (model: unknown) => Promise<boolean>;
		getThinkingLevel: () => string;
		setThinkingLevel: (level: string) => void;
	}

	export interface ExtensionContextActions {
		getModel: () => unknown;
		getScopedModels: () => readonly unknown[];
		isIdle: () => boolean;
		isProjectTrusted: () => boolean;
		getSignal: () => AbortSignal | undefined;
		abort: () => void;
		hasPendingMessages: () => boolean;
		shutdown: () => void;
		getContextUsage: () => unknown;
		compact: (options?: unknown) => void;
		getSystemPrompt: () => string;
	}

	export interface ExtensionCommandContextActions {
		waitForIdle: () => Promise<void>;
		newSession: (options?: {
			parentSession?: string;
			setup?: (sessionManager: SessionManagerSetupBridge) => Promise<void>;
			withSession?: (ctx: ReplacedSessionContext) => Promise<void>;
		}) => Promise<{ cancelled: boolean }>;
		fork: (
			entryId: string,
			options?: { position?: "before" | "at"; withSession?: (ctx: ReplacedSessionContext) => Promise<void> },
		) => Promise<{ cancelled: boolean; selectedText?: string }>;
		navigateTree: (
			targetId: string,
			options?: {
				summarize?: boolean;
				customInstructions?: string;
				replaceInstructions?: boolean;
				label?: string;
			},
		) => Promise<{
			cancelled: boolean;
			editorText?: string;
			aborted?: boolean;
			summaryEntry?: BranchSummaryEntry;
		}>;
		switchSession: (
			sessionPath: string,
			options?: { withSession?: (ctx: ReplacedSessionContext) => Promise<void> },
		) => Promise<{ cancelled: boolean }>;
		reload: () => Promise<void>;
	}

	export interface ExtensionRuntime {
		flagValues: Map<string, boolean | string>;
		pendingProviderRegistrations: Array<{ name: string; config: ProviderConfig; extensionPath: string }>;
		pendingNativeProviderRegistrations: Array<{ provider: Record<string, unknown>; extensionPath: string }>;
		registerProvider: (name: string, config: ProviderConfig, extensionPath?: string) => void;
		registerNativeProvider: (provider: Record<string, unknown>, extensionPath?: string) => void;
		unregisterProvider: (name: string, extensionPath?: string) => void;
		assertActive: () => void;
		invalidate: (message?: string) => void;
	}

	export class ExtensionRunner {
		constructor(
			extensions: Extension[],
			runtime: ExtensionRuntime,
			cwd: string,
			sessionManager: unknown,
			modelRegistry: unknown,
		);
		bindCore(
			actions: ExtensionActions,
			contextActions: ExtensionContextActions,
			providerActions?: {
				registerProvider?: (name: string, config: ProviderConfig) => void;
				registerNativeProvider?: (provider: ProviderConfig) => void;
				unregisterProvider?: (name: string) => void;
			},
		): void;
		bindCommandContext(actions?: ExtensionCommandContextActions): void;
		setUIContext(uiContext?: unknown, mode?: ExtensionMode): void;
		onError(listener: ExtensionErrorListener): () => void;
		emit(event: unknown): Promise<unknown>;
		emitContext(messages: unknown[]): Promise<unknown[]>;
		emitInput(text: string, images: unknown, source: string, streamingBehavior?: string): Promise<{ action: string; text?: string }>;
		emitMessageEnd(event: unknown): Promise<unknown>;
		emitResourcesDiscover(cwd: string, reason: string): Promise<unknown>;
		emitToolCall(event: ToolCallEvent): Promise<ToolCallEventResult | undefined>;
		emitToolResult(event: ToolResultEvent): Promise<ToolResultEventResult | undefined>;
		emitBeforeProviderHeaders(headers: Record<string, string | null>): Promise<Record<string, string | null>>;
		emitBeforeAgentStart(
			prompt: string,
			images: unknown[] | undefined,
			systemPrompt: string,
			systemPromptOptions: BuildSystemPromptOptions,
		): Promise<BeforeAgentStartCombinedResult | undefined>;
		getCommand(name: string): ResolvedCommand | undefined;
		getRegisteredCommands(): ResolvedCommand[];
		getToolDefinition(toolName: string): ToolDefinition | undefined;
		getAllRegisteredTools(): RegisteredTool[];
		getFlags(): Map<string, ExtensionFlag>;
		setFlagValue(name: string, value: boolean | string): void;
		getFlagValues(): Map<string, boolean | string>;
		hasHandlers(eventType: string): boolean;
		invalidate(message?: string): void;
		shutdown(): void;
		createContext(): ExtensionContext;
		createCommandContext(): ExtensionCommandContext;
	}

	export function createExtensionRuntime(): ExtensionRuntime;
	export function loadExtensionFromFactory(
		factory: ExtensionFactory,
		cwd: string,
		eventBus: unknown,
		runtime: ExtensionRuntime,
		extensionPath?: string,
	): Promise<Extension>;
}

declare module "@earendil-works/pi-ai" {
	export const Type: {
		Object(properties: Record<string, unknown>, options?: Record<string, unknown>): Record<string, unknown>;
		String(options?: Record<string, unknown>): unknown;
		Number(options?: Record<string, unknown>): unknown;
		Boolean(options?: Record<string, unknown>): unknown;
		Optional(schema: unknown, options?: Record<string, unknown>): unknown;
	};

	export class AssistantMessageEventStream implements AsyncIterable<unknown> {
		push(event: unknown): void;
		end(result?: unknown): void;
		result(): Promise<unknown>;
		[Symbol.asyncIterator](): AsyncIterator<unknown>;
	}

	export function createAssistantMessageEventStream(): AssistantMessageEventStream;

	/** Bridge-local content block shapes mirroring the pinned pi-ai types. */
	export interface TextContent {
		type: "text";
		text: string;
	}
	export interface ImageContent {
		type: "image";
		data: string;
		mimeType: string;
	}
	export interface ThinkingContent {
		type: "thinking";
		thinking: string;
	}
	export interface ToolCall {
		type: "toolCall";
		id: string;
		name: string;
		arguments: Record<string, unknown>;
	}
	export interface Usage {
		input: number;
		output: number;
		cacheRead: number;
		cacheWrite: number;
		totalTokens: number;
		cost: {
			input: number;
			output: number;
			cacheRead: number;
			cacheWrite: number;
			total: number;
		};
	}
	export type StopReason = "stop" | "length" | "toolUse" | "error" | "aborted";

	/** Bridge-local message shapes mirroring the pinned pi-ai types. */
	export interface AssistantMessage {
		role: "assistant";
		content: (TextContent | ThinkingContent | ToolCall)[];
		api: string;
		provider: string;
		model: string;
		usage: Usage;
		stopReason: StopReason;
		errorMessage?: string;
		timestamp: number;
	}
	export interface UserMessage {
		role: "user";
		content: string | (TextContent | ImageContent)[];
		timestamp: number;
	}
	export interface ToolResultMessage {
		role: "toolResult";
		toolCallId: string;
		toolName: string;
		content: (TextContent | ImageContent)[];
		isError: boolean;
		timestamp: number;
	}
	export type Message = UserMessage | AssistantMessage | ToolResultMessage;

	export interface Context {
		systemPrompt?: string;
		messages: Message[];
		tools?: unknown[];
	}
	export interface Model<TApi extends string = string> {
		id: string;
		name: string;
		api: TApi;
		provider: string;
		baseUrl: string;
		reasoning: boolean;
		input: ("text" | "image")[];
		cost: unknown;
		contextWindow: number;
		maxTokens: number;
	}
	export interface SimpleStreamOptions {
		signal?: AbortSignal;
		[key: string]: unknown;
	}

	export function generateImages(
		model: unknown,
		context: unknown,
		options?: unknown,
	): Promise<unknown>;
	export function getImageProviders(): string[];
	export function getImagesApiProvider(api: string):
		| { api: string; generateImages: (...args: never[]) => Promise<unknown> }
		| undefined;
}

declare module "@earendil-works/pi-ai/compat" {
	export function validateToolArguments(
		tool: { name: string; parameters: unknown },
		call: { name: string; arguments: unknown },
	): Record<string, unknown>;
}

declare module "@earendil-works/pi-ai/utils/json-parse.ts" {
	export function parseStreamingJson<T = Record<string, unknown>>(
		partialJson: string | undefined,
	): T;
}

declare module "typebox" {
	export const Type: {
		Object(properties: Record<string, unknown>, options?: Record<string, unknown>): Record<string, unknown>;
		String(options?: Record<string, unknown>): unknown;
		Number(options?: Record<string, unknown>): unknown;
		Boolean(options?: Record<string, unknown>): unknown;
		Optional(schema: unknown, options?: Record<string, unknown>): unknown;
	};
}

declare module "typebox/value" {
	export const Value: {
		Check(schema: unknown, value: unknown): boolean;
	};
}

declare module "typebox/compile" {
	export const Compile: (schema: unknown) => { Check(value: unknown): boolean };
}

declare module "@sinclair/typebox" {
	export { Type } from "typebox";
}

declare module "@sinclair/typebox/value" {
	export { Value } from "typebox/value";
}

declare module "@sinclair/typebox/compile" {
	export { Compile } from "typebox/compile";
}

declare module "@earendil-works/pi-coding-agent/builtins" {
	import type { InlineExtension } from "@earendil-works/pi-coding-agent";
	export const builtInExtensions: InlineExtension[];
}

// Opaque bundles passed straight to jiti virtualModules (no typed surface
// needed host-side); runtime resolution follows the `tsconfig` path mappings.
declare module "pi-coding-agent-full";
declare module "@earendil-works/pi-agent-core";
declare module "@earendil-works/pi-tui";
declare module "@earendil-works/pi-ai/oauth";
declare module "@earendil-works/pi-ai/providers/all";
