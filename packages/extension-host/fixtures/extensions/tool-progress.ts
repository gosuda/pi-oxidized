/**
 * Tool fixture with progress updates + optional hang for cancellation.
 *
 * - args.mode === "progress": emits one onUpdate then final result
 * - args.mode === "cancel": waits until AbortSignal aborts
 * - otherwise: echoes text
 */
import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const progressTool = defineTool({
	name: "progress_echo",
	label: "ProgressEcho",
	description: "Echoes with optional progress and cancel modes",
	parameters: Type.Object({
		text: Type.String({ description: "Text to echo" }),
		mode: Type.Optional(Type.String({ description: "progress | cancel | plain" })),
	}),
	async execute(toolCallId, params, signal, onUpdate) {
		const text = String(params.text ?? "");
		const mode = typeof params.mode === "string" ? params.mode : "plain";

		if (mode === "progress") {
			onUpdate?.({
				content: [{ type: "text", text: `partial:${text}` }],
				details: { stage: "partial", toolCallId },
			});
			return {
				content: [{ type: "text", text: `final:${text}` }],
				details: { stage: "final", toolCallId },
			};
		}

		if (mode === "cancel") {
			await new Promise<void>((_resolve, reject) => {
				if (signal?.aborted) {
					reject(new Error("aborted"));
					return;
				}
				const onAbort = () => reject(new Error("aborted"));
				signal?.addEventListener("abort", onAbort, { once: true });
			});
		}

		return {
			content: [{ type: "text", text }],
			details: { echoed: text, toolCallId },
		};
	},
});

export default function toolProgressExtension(pi: ExtensionAPI): void {
	pi.registerTool(progressTool);
	pi.registerCommand("progress_cmd", {
		description: "Marker command for registry snapshot",
		async handler() {},
	});
	pi.registerFlag("progress-flag", {
		description: "Marker flag",
		type: "boolean",
		default: false,
	});
	pi.registerShortcut("ctrl+p", {
		description: "Marker shortcut",
		handler() {},
	});
	pi.registerMessageRenderer("progress_msg", () => ({
		render: () => ["progress"],
	}));
	pi.on("session_start", () => {});
	pi.on("agent_start", () => {});
}
