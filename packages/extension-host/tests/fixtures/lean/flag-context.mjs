/**
 * Lean fixture that surfaces the effective flag values handed to callbacks
 * via `ctx.flags`, so tests can assert that `flags.set` reaches extension
 * code (not just the later registry snapshot).
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "lean-flag-context",
	commands: [
		{
			name: "report-flags",
			description: "Record the effective flag values from ctx",
			handler: (_args, ctx) => {
				mark("flags", { ...ctx.flags });
			},
		},
	],
	tools: [
		{
			name: "flag-tool",
			description: "Echoes the effective flag values",
			parameters: { type: "object", properties: {} },
			execute: (_args, ctx) => {
				mark("tool-flags", { ...ctx.flags });
				return { content: [{ type: "text", text: "ok" }] };
			},
		},
	],
	flags: [
		{
			name: "mode",
			description: "Operating mode",
			type: "string",
			default: "quiet",
		},
		{
			name: "debug",
			description: "Debug output",
			type: "boolean",
			default: false,
		},
	],
};
