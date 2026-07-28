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
	},
};
