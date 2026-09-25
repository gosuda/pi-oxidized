/**
 * Boundary fold fixture whose handlers return nothing, proving the runner
 * still revalidates through Rust after each handler and preserves the
 * running drafts (a handler may enqueue messages without returning drafts).
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "boundary-noop",
	hooks: {
		turn_end: (event) => {
			mark("noop.turn_end", {
				entries: event.entries,
				continue: event.continue,
				context: event.context,
			});
			return undefined;
		},
		agent_before_settle: (event) => {
			mark("noop.agent_before_settle", {
				entries: event.entries,
				context: event.context,
			});
			return undefined;
		},
	},
};
