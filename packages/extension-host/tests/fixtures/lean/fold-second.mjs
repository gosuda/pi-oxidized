/**
 * Ordered-fold fixture (second in load order). Its recorded values are the
 * regression witness: they must show the first fixture's modifications,
 * not the original payload. Transform results deliberately omit fields
 * (input images, tool_result details, tool_result terminate) to prove
 * running values survive partial updates.
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "fold-second",
	hooks: {
		input: (event) => {
			mark("second.input", { text: event.text, images: event.images });
			// images omitted on purpose: the running images must be preserved.
			return { action: "transform", text: `${event.text}|second` };
		},
		before_agent_start: (event) => {
			mark("second.before_agent_start", event.systemPrompt);
			return { systemPrompt: `${event.systemPrompt}|second` };
		},
		tool_result: (event) => {
			mark("second.tool_result", {
				content: event.content,
				details: event.details,
				isError: event.isError,
				terminate: event.terminate,
			});
			// details and terminate omitted on purpose: the first fixture's
			// running values survive.
			return {
				content: [...(event.content ?? []), "second"],
				isError: false,
			};
		},
		message_end: (event) => {
			mark("second.message_end", event.message);
			return {
				message: { ...event.message, content: `${event.message.content}|second` },
			};
		},
	},
};
