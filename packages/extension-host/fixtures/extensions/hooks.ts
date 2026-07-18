/**
 * Hooks fixture: registers handlers for representative lifecycle events
 * across all families (session, agent, message, tool, input, context).
 * Used by the host/runner test to verify the REAL ExtensionRunner dispatches
 * hooks and merges results correctly.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function hooksExtension(pi: ExtensionAPI): void {
	const seen: string[] = [];

	pi.on("session_start", (event, ctx) => {
		seen.push(`session_start:${event.reason}`);
		void ctx;
	});

	pi.on("agent_start", () => {
		seen.push("agent_start");
	});

	pi.on("message_end", (event) => {
		// Control hook: return the message unchanged (pipeline merge test).
		return { message: event.message };
	});

	pi.on("context", (event) => {
		// Control hook: pipeline — return messages unchanged.
		return { messages: event.messages };
	});

	pi.on("input", (event) => {
		// Control hook: pass through unchanged.
		seen.push(`input:${event.source}`);
		return { action: "continue" as const };
	});

	pi.on("turn_start", (event) => {
		seen.push(`turn_start:${event.turnIndex}`);
	});

	pi.on("tool_execution_start", (event) => {
		seen.push(`tool_exec:${event.toolName}`);
	});
}
