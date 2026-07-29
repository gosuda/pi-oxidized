/**
 * Ordered-fold fixture (first in load order). Records the values each
 * foldable hook received, then returns a deterministic modification the
 * second fixture must observe through the event — proving the runner
 * threads running values instead of the original payload.
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "fold-first",
	hooks: {
		input: (event) => {
			mark("first.input", { text: event.text, images: event.images });
			return {
				action: "transform",
				text: `${event.text}|first`,
				images: [...(event.images ?? []), { marker: "first" }],
			};
		},
		before_agent_start: (event) => {
			mark("first.before_agent_start", event.systemPrompt);
			return { systemPrompt: `${event.systemPrompt}|first` };
		},
		tool_result: (event) => {
			mark("first.tool_result", {
				content: event.content,
				details: event.details,
				isError: event.isError,
			});
			return {
				content: [...(event.content ?? []), "first"],
				details: { ...(event.details ?? {}), first: true },
				isError: true,
			};
		},
		message_end: (event) => {
			mark("first.message_end", event.message);
			return {
				message: { ...event.message, content: `${event.message.content}|first` },
			};
		},
	},
};
