/**
 * Second-handler fixture for the T10 true→explicit-false terminate fold.
 * Observes the running terminate from fold-first, then overrides with false.
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "fold-terminate-false",
	hooks: {
		tool_result: (event) => {
			mark("second.tool_result", {
				content: event.content,
				details: event.details,
				isError: event.isError,
				terminate: event.terminate,
			});
			return { terminate: false };
		},
	},
};
