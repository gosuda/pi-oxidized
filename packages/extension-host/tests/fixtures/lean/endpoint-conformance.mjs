function observe(event) {
	const log = globalThis.__endpointConformanceLog ?? [];
	log.push(structuredClone(event));
	globalThis.__endpointConformanceLog = log;
}

export default {
	name: "endpoint-conformance",
	hooks: {
		message_update: (event) => {
			observe(event);
		},
		before_agent_start: (event) => {
			observe({
				type: event.type,
				systemPrompt: event.systemPrompt,
				cwd: event.systemPromptOptions.cwd,
			});
			const message = { role: "user", content: "injected" };
			if (event.prompt === "no-system-prompt") return { message };
			if (event.prompt === "non-string-system-prompt") {
				return { message, systemPrompt: null };
			}
			return {
				message,
				systemPrompt: `${event.systemPrompt}|${event.systemPromptOptions.cwd}`,
			};
		},
		tool_call: (event) => {
			observe({ type: event.type, toolName: event.toolName, toolCallId: event.toolCallId, input: event.input });
			event.input.fromHook = "tool-call";
			if (event.toolCallId === "terminate-call") {
				return { block: true, reason: "terminate test", terminate: true };
			}
		},
		tool_result: (event) => {
			observe({
				type: event.type,
				toolName: event.toolName,
				toolCallId: event.toolCallId,
				input: event.input,
				content: event.content,
				details: event.details,
				isError: event.isError,
			});
			return {
				content: [{ type: "text", text: "rewritten tool result" }],
				details: { fromHook: true },
				isError: true,
			};
		},
		message_end: (event) => {
			observe({ type: event.type, message: event.message });
			return { message: { role: "assistant", content: [{ type: "text", text: "rewritten message" }] } };
		},
		input: (event) => {
			observe({ type: event.type, text: event.text, images: event.images, source: event.source });
			if (event.text === "handled-text") return { action: "handled" };
			return { action: "transform", text: `${event.text} rewritten` };
		},
		resources_discover: (event) => {
			observe({ type: event.type, cwd: event.cwd, reason: event.reason });
			return { skillPaths: ["/skills"], promptPaths: ["/prompts"], themePaths: ["/themes"] };
		},
		session_before_tree: (event) => {
			observe({ type: event.type });
			return { cancel: true, reason: "endpoint conformance" };
		},
		before_provider_headers: (event) => {
			observe({ type: event.type, headers: event.headers });
			// In-place mutation: null deletes a header, new keys are added.
			delete event.headers["X-Delete-Me"];
			event.headers["X-Injected"] = "from-hook";
		},
	},
	shortcuts: [
		{
			key: "ctrl+shift+e",
			handler: () => {
				observe({ type: "shortcut", key: "ctrl+shift+e" });
				return new Promise((resolve) => setImmediate(resolve));
			},
		},
	],
};
