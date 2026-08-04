import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import type {
	AssistantMessage,
	Context,
	Model,
	SimpleStreamOptions,
	ToolCall,
} from "@earendil-works/pi-ai";
import { createAssistantMessageEventStream } from "./runtime.ts";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export const VERIFICATION_PROVIDER = "verification";
export const VERIFICATION_MODEL = "model";
export const DEFAULT_FINAL_MARKER = "PI_VERIFICATION_FINAL";

const ENV = {
	mode: "PI_VERIFICATION_MODE",
	chunkCount: "PI_VERIFICATION_CHUNK_COUNT",
	chunkDelayMs: "PI_VERIFICATION_CHUNK_DELAY_MS",
	finalMarker: "PI_VERIFICATION_FINAL_MARKER",
	loadCountPath: "PI_VERIFICATION_LOAD_COUNT_PATH",
	toolPath: "PI_VERIFICATION_TOOL_PATH",
	compatibilityPath: "PI_VERIFICATION_COMPATIBILITY_PATH",
} as const;

export const VERIFICATION_PROFILE_FLAG = "verification-profile";
export const VERIFICATION_SHORTCUT = "ctrl+shift+x";
export const VERIFICATION_DIALOG_COMMAND = "verification-dialogs";
export const VERIFICATION_CUSTOM_UI_COMMAND = "verification-custom-ui";
export const VERIFICATION_FLAG_COMMAND = "verification-observe-flag";
export const VERIFICATION_SESSION_REPLACEMENT_COMMAND = "verification-session-replacement";

const COMPATIBILITY_INSTANCE = `${process.pid}:${Date.now()}`;
let compatibilitySequence = 0;

function recordCompatibility(stage: string, value: unknown): void {
	const path = process.env[ENV.compatibilityPath];
	if (!path) return;
	mkdirSync(dirname(path), { recursive: true });
	appendFileSync(
		path,
		`${JSON.stringify({
			stage,
			instance: COMPATIBILITY_INSTANCE,
			sequence: ++compatibilitySequence,
			value: value ?? null,
		})}\n`,
		"utf8",
	);
}

type VerificationMode = "auto" | "text" | "tools" | "compaction";

interface VerificationConfig {
	mode: VerificationMode;
	chunkCount: number;
	chunkDelayMs: number;
	finalMarker: string;
	toolPath: string;
}

function boundedInteger(name: string, fallback: number, maximum: number): number {
	const raw = process.env[name];
	if (raw === undefined) return fallback;
	if (!/^\d+$/.test(raw)) throw new Error(`${name} must be a non-negative integer`);
	const value = Number(raw);
	if (!Number.isSafeInteger(value) || value > maximum) throw new Error(`${name} must be <= ${maximum}`);
	return value;
}

function configFromEnvironment(): VerificationConfig {
	const rawMode = process.env[ENV.mode] ?? "auto";
	if (rawMode !== "auto" && rawMode !== "text" && rawMode !== "tools" && rawMode !== "compaction") {
		throw new Error(`${ENV.mode} must be auto, text, tools, or compaction`);
	}
	return {
		mode: rawMode,
		chunkCount: boundedInteger(ENV.chunkCount, 1, 100_000),
		chunkDelayMs: boundedInteger(ENV.chunkDelayMs, 0, 60_000),
		finalMarker: process.env[ENV.finalMarker] ?? DEFAULT_FINAL_MARKER,
		toolPath: process.env[ENV.toolPath] ?? "verification-e2e.txt",
	};
}

function recordLoadGeneration(): void {
	const path = process.env[ENV.loadCountPath];
	if (!path) return;
	let generation = 0;
	try {
		const parsed = Number.parseInt(readFileSync(path, "utf8").trim(), 10);
		if (Number.isSafeInteger(parsed) && parsed >= 0) generation = parsed;
	} catch (error) {
		if (!(error instanceof Error && "code" in error && error.code === "ENOENT")) throw error;
	}
	mkdirSync(dirname(path), { recursive: true });
	writeFileSync(path, `${generation + 1}\n`, "utf8");
}

function emptyUsage(): AssistantMessage["usage"] {
	return {
		input: 0,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 0,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
	};
}

function message(model: Model<any>, content: AssistantMessage["content"], stopReason: AssistantMessage["stopReason"]): AssistantMessage {
	return {
		role: "assistant",
		content,
		api: model.api,
		provider: model.provider,
		model: model.id,
		usage: emptyUsage(),
		stopReason,
		timestamp: 0,
	};
}

function contextText(context: Context): string {
	return context.messages
		.flatMap((entry) => {
			if (entry.role !== "user") return [];
			if (typeof entry.content === "string") return [entry.content];
			return entry.content.flatMap((block) => (block.type === "text" ? [block.text] : []));
		})
		.join("\n");
}

function modeFor(config: VerificationConfig, context: Context): Exclude<VerificationMode, "auto"> {
	if (config.mode !== "auto") return config.mode;
	const text = contextText(context);
	if (text.includes("The messages above are a conversation to summarize") || text.includes("<previous-summary>")) {
		return "compaction";
	}
	return text.includes("verification:tools") ? "tools" : "text";
}

function nextTool(context: Context, toolPath: string): ToolCall | undefined {
	const completed = new Set(
		context.messages.flatMap((entry) => (entry.role === "toolResult" ? [entry.toolName] : [])),
	);
	if (!completed.has("read")) {
		return { type: "toolCall", id: "verification-read", name: "read", arguments: { path: toolPath } };
	}
	if (!completed.has("edit")) {
		return {
			type: "toolCall",
			id: "verification-edit",
			name: "edit",
			arguments: { path: toolPath, oldText: "verification-before", newText: "verification-after" },
		};
	}
	if (!completed.has("bash")) {
		return {
			type: "toolCall",
			id: "verification-bash",
			name: "bash",
			arguments: { command: `printf '%s\\n' verification-bash` },
		};
	}
	return undefined;
}

function textFor(config: VerificationConfig, mode: "text" | "compaction"): string {
	if (mode === "compaction") {
		return `## Goal\nDeterministic verification compaction.\n\n## Progress\n- Fixture summary generated.\n\n## Next Steps\n- Continue verification.\n\n${config.finalMarker}`;
	}
	const chunks = Array.from({ length: config.chunkCount }, (_, index) => `verification-chunk-${String(index + 1).padStart(4, "0")}\n`);
	return `${chunks.join("")}${config.finalMarker}`;
}

function streamVerification(model: Model<any>, context: Context, options?: SimpleStreamOptions) {
	const stream = createAssistantMessageEventStream();
	const config = configFromEnvironment();
	void (async () => {
		try {
			options?.signal?.throwIfAborted();
			const mode = modeFor(config, context);
			const toolCall = mode === "tools" ? nextTool(context, config.toolPath) : undefined;
			if (toolCall) {
				const partial = message(model, [toolCall], "toolUse");
				stream.push({ type: "start", partial: message(model, [], "toolUse") });
				stream.push({ type: "toolcall_start", contentIndex: 0, partial });
				const delta = JSON.stringify(toolCall.arguments);
				stream.push({ type: "toolcall_delta", contentIndex: 0, delta, partial });
				stream.push({ type: "toolcall_end", contentIndex: 0, toolCall, partial });
				stream.push({ type: "done", reason: "toolUse", message: partial });
				stream.end();
				return;
			}

			const text = textFor(config, mode === "compaction" ? "compaction" : "text");
			const partial = message(model, [{ type: "text", text: "" }], "stop");
			stream.push({ type: "start", partial: message(model, [], "stop") });
			stream.push({ type: "text_start", contentIndex: 0, partial });
			const pieces = mode === "compaction" ? [text] : [...text.matchAll(/verification-chunk-\d{4}\n/g)].map((match) => match[0]);
			if (mode !== "compaction") pieces.push(config.finalMarker);
			for (const piece of pieces) {
				options?.signal?.throwIfAborted();
				if (config.chunkDelayMs > 0) await Bun.sleep(config.chunkDelayMs);
				const block = partial.content[0];
				if (!block || block.type !== "text") throw new Error("verification text block missing");
				block.text += piece;
				stream.push({ type: "text_delta", contentIndex: 0, delta: piece, partial });
			}
			stream.push({ type: "text_end", contentIndex: 0, content: text, partial });
			stream.push({ type: "done", reason: "stop", message: partial });
			stream.end();
		} catch (error) {
			const reason = options?.signal?.aborted === true ? "aborted" : "error";
			const failed = message(model, [], reason);
			failed.errorMessage = error instanceof Error ? error.message : String(error);
			stream.push({ type: "error", reason, error: failed });
			stream.end();
		}
	})();
	return stream;
}

export default function verificationExtension(pi: ExtensionAPI): void {
	recordLoadGeneration();

	pi.registerFlag(VERIFICATION_PROFILE_FLAG, {
		description: "Run-local compatibility verification profile",
		type: "string",
	});

	pi.on("session_start", () => {
		recordCompatibility("session_start.before", { flag: VERIFICATION_PROFILE_FLAG });
		recordCompatibility("session_start.after", { value: pi.getFlag(VERIFICATION_PROFILE_FLAG) ?? null });
	});

	pi.registerShortcut(VERIFICATION_SHORTCUT, {
		description: "Record compatibility shortcut dispatch",
		handler: () => {
			recordCompatibility("shortcut.before", { shortcut: VERIFICATION_SHORTCUT });
			recordCompatibility("shortcut.after", { shortcut: VERIFICATION_SHORTCUT, dispatched: true });
		},
	});

	pi.registerCommand(VERIFICATION_FLAG_COMMAND, {
		description: "Record the current verification profile flag",
		handler: async () => {
			recordCompatibility("flag_observation.before", { flag: VERIFICATION_PROFILE_FLAG });
			recordCompatibility("flag_observation.after", { value: pi.getFlag(VERIFICATION_PROFILE_FLAG) ?? null });
			recordCompatibility("replacement.post.before", {});
			try {
				pi.sendMessage({
					customType: "verification-post-replacement",
					content: "verification post replacement",
					display: false,
				});
				recordCompatibility("replacement.post", {});
			} catch (error) {
				recordCompatibility("replacement.post.error", {
					message: error instanceof Error ? error.message : String(error),
				});
				throw error;
			}
		},
	});

	pi.registerCommand(VERIFICATION_SESSION_REPLACEMENT_COMMAND, {
		description: "Exercise real session replacement lifecycle",
		handler: async (_args, ctx) => {
			recordCompatibility("replacement.before", { command: VERIFICATION_SESSION_REPLACEMENT_COMMAND });
			const result = await ctx.newSession({
				setup: async (sessionManager) => {
					await sessionManager.appendCustomEntry("verification-replacement-setup", { source: "setup" });
					recordCompatibility("replacement.setup", { source: "setup" });
				},
				withSession: async (replacementCtx) => {
					recordCompatibility("replacement.withSession.before", {});
					await replacementCtx.sendMessage({
						customType: "verification-replacement-with-session",
						content: "verification replacement withSession",
						display: false,
					});
					recordCompatibility("replacement.withSession.after", {});
				},
			});
			recordCompatibility("replacement.after", { cancelled: result.cancelled });
		},
	});


	pi.registerCommand(VERIFICATION_DIALOG_COMMAND, {
		description: "Exercise real extension dialogs",
		handler: async (_args, ctx) => {
			const operationId = "verification-dialogs-v1";
			recordCompatibility("dialogs.command.before", { operationId });

			recordCompatibility("dialogs.select.before", { operationId, options: ["alpha", "beta"] });
			const select = await ctx.ui.select("Verification select prompt", ["alpha", "beta"]);
			recordCompatibility("dialogs.select.after", { operationId, value: select ?? null });

			recordCompatibility("dialogs.confirm.before", { operationId });
			const confirm = await ctx.ui.confirm("Verification confirm prompt", "Choose Yes");
			recordCompatibility("dialogs.confirm.after", { operationId, value: confirm });

			recordCompatibility("dialogs.input.before", { operationId });
			const input = await ctx.ui.input("Verification input prompt", "dialog input");
			recordCompatibility("dialogs.input.after", { operationId, value: input ?? null });

			recordCompatibility("dialogs.editor.before", { operationId });
			const editor = await ctx.ui.editor("Verification editor prompt");
			recordCompatibility("dialogs.editor.after", { operationId, value: editor ?? null });

			const results = { operationId, select: select ?? null, confirm, input: input ?? null, editor: editor ?? null };
			recordCompatibility("dialogs.results", results);
			recordCompatibility("dialogs.command.after", results);
		},
	});

	pi.registerCommand(VERIFICATION_CUSTOM_UI_COMMAND, {
		description: "Exercise real focusable custom extension UI",
		handler: async (_args, ctx) => {
			recordCompatibility("custom.command.before", { state: "initial" });
			const result = await ctx.ui.custom<string>((tui, _theme, _keybindings, done) => {
				let state = "initial";
				let initialRendered = false;
				let updatedRendered = false;
				let completionTimer: ReturnType<typeof setTimeout> | undefined;
				return {
					focused: false,
					invalidate() {},
					render(): string[] {
						if (state === "initial" && !initialRendered) {
							initialRendered = true;
							recordCompatibility("custom.render.initial", { state });
						}
						if (state === "updated" && !updatedRendered) {
							updatedRendered = true;
							recordCompatibility("custom.render.updated", { state });
							completionTimer = setTimeout(() => done(state), 200);
						}
						return [`Verification custom state=${state}`];
					},
					handleInput(data: string): void {
						if (data !== "x" || state !== "initial") return;
						recordCompatibility("custom.input.before", { input: data, state });
						state = "updated";
						recordCompatibility("custom.input.after", { input: data, state });
						tui.requestRender();
					},
					dispose(): void {
						if (completionTimer !== undefined) clearTimeout(completionTimer);
						recordCompatibility("custom.dispose", { state });
					},
				};
			});
			recordCompatibility("custom.command.after", { state: result });
		},
	});

	pi.registerProvider(VERIFICATION_PROVIDER, {
		name: "Verification",
		baseUrl: "https://verification.invalid",
		api: "verification",
		models: [
			{
				id: VERIFICATION_MODEL,
				name: "Verification Model",
				reasoning: false,
				input: ["text"],
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
				contextWindow: 1_000_000,
				maxTokens: 100_000,
			},
		],
		streamSimple: streamVerification,
	});
}
