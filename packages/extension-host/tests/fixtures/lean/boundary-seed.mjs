/**
 * Boundary fold fixture (first in load order). Replaces the running drafts
 * with one custom entry and requests continuation, so a later boundary
 * handler must observe — and replace — the entire running fold state.
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "boundary-seed",
	hooks: {
		turn_end: (event) => {
			mark("seed.turn_end", {
				entries: event.entries,
				continue: event.continue,
				context: event.context,
			});
			return {
				entries: [{ type: "custom", customType: "seed-draft", data: { ok: true } }],
				continue: true,
			};
		},
	},
};
