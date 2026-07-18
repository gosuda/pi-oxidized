/**
 * Tool + command + widget fixture: exercises registerTool, registerCommand,
 * and setWidget(string[]) to verify the runner collects all registrations
 * and the host can render widgets to structured runs.
 */

import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const echoTool = defineTool({
	name: "echo",
	label: "Echo",
	description: "Echoes back the input text",
	parameters: Type.Object({
		text: Type.String({ description: "Text to echo" }),
	}),
	async execute(_toolCallId, params) {
		const text = String(params.text);
		return {
			content: [{ type: "text" as const, text }],
			details: { echoed: text },
		};
	},
});

export default function toolExtension(pi: ExtensionAPI): void {
	pi.registerTool(echoTool);

	pi.registerCommand("greet", {
		description: "Print a greeting",
		async handler(_args, ctx) {
			ctx.ui.notify("Hello from extension!", "info");
		},
	});

	pi.registerCommand("showOverlay", {
		description: "Show a custom overlay",
		async handler(args, ctx) {
			await ctx.ui.custom((_tui, _theme, _keybindings, done) => {
				// Wait for an IPC event to call done() to avoid timers.
				pi.on("session_info_changed", () => done(args));
				return { render: () => ["overlay content"] };
			});
		},
	});

	pi.on("session_start", (_event, ctx) => {
		if (!ctx.hasUI) return;
		ctx.ui.setWidget("widget.status", [
			"\x1b[32m●\x1b[0m \x1b[1mready\x1b[0m",
			"\x1b[34mtools: 1\x1b[0m",
		]);
	});
}
