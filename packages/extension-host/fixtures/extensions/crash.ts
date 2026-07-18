/**
 * Crash fixture: throws in specific handlers and tool execute to test
 * exactly-once error isolation. The runner must catch, emit extensionError
 * with retryable=false, and never replay the failed effect.
 */

import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const crashTool = defineTool({
	name: "crash_tool",
	label: "CrashTool",
	description: "Always throws to test tool-execute error isolation",
	parameters: Type.Object({}),
	async execute() {
		throw new Error("crash-in-tool-execute");
	},
});

export default function crashExtension(pi: ExtensionAPI): void {
	pi.on("session_start", () => {
		throw new Error("crash-in-session-start");
	});
	pi.on("agent_start", () => {
		throw new Error("crash-in-agent-start");
	});
	pi.on("message_end", () => {
		throw new Error("crash-in-message-end");
	});
	pi.registerTool(crashTool);
	pi.registerProvider("crash_provider", {
		streamSimple: () => {
			throw new Error("crash-in-provider-stream");
		},
	});
}
