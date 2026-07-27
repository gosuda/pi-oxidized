/**
 * Prebundled-style lean extension fixture (Mode 2). Plain object literal —
 * no imports at all, exactly what a bundled `.mjs` entry looks like after
 * the lean API is inlined by the extension author's bundler.
 *
 * Behaviors are observable over the protocol or via globalThis markers so
 * both the in-process suite and the subprocess suite can assert them.
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "lean-echo",
	tools: [
		{
			name: "echo",
			label: "Echo",
			description: "Echo the input text back",
			parameters: {
				type: "object",
				properties: { text: { type: "string" } },
				required: ["text"],
			},
			prepare: (args) => ({ ...args, preparedBy: "lean" }),
			validate: (args) => {
				if (typeof args?.text !== "string") {
					throw new Error("echo.text must be a string");
				}
				mark("validate", args);
				return { ...args, validatedBy: "lean" };
			},
			execute: (args, ctx) => {
				ctx.onUpdate({ content: [{ type: "text", text: "echoing…" }] });
				mark("execute", { args, toolCallId: ctx.toolCallId, cwd: ctx.cwd });
				return {
					content: [{ type: "text", text: `echo:${args.text}` }],
					details: { preparedBy: args.preparedBy, extensionPath: ctx.extensionPath },
				};
			},
		},
		{
			name: "slow",
			description: "Waits until aborted (cancel fixture)",
			parameters: { type: "object", properties: {} },
			execute: (_args, ctx) => {
				// Signal via the wire that the execute handler is parked, so the
				// cancel test synchronizes on a real event instead of a sleep.
				ctx.onUpdate({ content: [{ type: "text", text: "slow:started" }] });
				return new Promise((_resolve, reject) => {
					const onAbort = () => reject(new Error("slow tool aborted"));
					if (ctx.signal.aborted) {
						onAbort();
						return;
					}
					ctx.signal.addEventListener("abort", onAbort, { once: true });
				});
			},
		},
	],
	commands: [
		{
			name: "greet",
			description: "Record a greeting",
			handler: (args, ctx) => {
				mark("command", { args, cwd: ctx.cwd });
			},
		},
	],
	flags: [
		{
			name: "verbose",
			description: "Verbose output",
			type: "boolean",
			default: false,
		},
	],
	shortcuts: [
		{
			key: "ctrl+alt+e",
			description: "Run the echo shortcut",
			handler: (ctx) => {
				mark("shortcut", { cwd: ctx.cwd });
			},
		},
	],
	providers: [
		{
			name: "lean-provider",
			displayName: "Lean Provider",
			baseUrl: "https://example.invalid",
			streamSimple: async function* (model, context, options) {
				mark("provider.stream", { model, hasSignal: typeof options?.signal?.aborted === "boolean" });
				yield { type: "start", partial: { role: "assistant", content: [] } };
				// Cancel fixture: park after the start event until the host
				// aborts options.signal (mirrors the slow tool).
				if (model?.id === "slow") {
					await new Promise((_resolve, reject) => {
						const signal = options?.signal;
						if (signal?.aborted) {
							reject(new Error("provider stream aborted"));
							return;
						}
						signal?.addEventListener(
							"abort",
							() => reject(new Error("provider stream aborted")),
							{ once: true },
						);
					});
				}
				yield { type: "done", reason: "stop", message: { role: "assistant", content: [] } };
			},
		},
	],
	hooks: {
		session_start: () => {
			mark("hook.session_start", true);
		},
		tool_call: (event) => {
			event.input["patched"] = true;
			return { block: false };
		},
		input: () => ({ action: "continue" }),
		message_update: (event) => {
			mark("hook.message_update", event.assistantMessageEvent?.type);
		},
	},
};
