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
			return {
				message: { role: "user", content: "injected" },
				systemPrompt: `${event.systemPrompt}|${event.systemPromptOptions.cwd}`,
			};
		},
	},
};
