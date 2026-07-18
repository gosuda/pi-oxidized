import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import type {
	AssistantMessage,
	Context,
	Model,
	SimpleStreamOptions,
	ToolCall,
} from "../../.references/pi/packages/ai/src/index.ts";
import { createAssistantMessageEventStream } from "../../.references/pi/packages/ai/src/index.ts";
import type { ExtensionAPI } from "../../.references/pi/packages/coding-agent/src/index.ts";

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
} as const;

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
