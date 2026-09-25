/**
 * Boundary fold fixture returning a draft the Rust projection will reject
 * (the test's preview responder answers the correlated preview with an
 * error), proving invalid drafts are reported and cannot persist.
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "boundary-invalid",
	hooks: {
		turn_end: (event) => {
			mark("invalid.turn_end", {
				entries: event.entries,
				context: event.context,
			});
			return {
				entries: [{ type: "custom", customType: "poison-draft" }],
			};
		},
	},
};
