/**
 * Boundary fold fixture that replaces invalid drafts left by an earlier
 * handler with a valid draft, proving later handlers can correct the fold
 * after a failed preview.
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "boundary-correct",
	hooks: {
		turn_end: (event) => {
			mark("correct.turn_end", {
				entries: event.entries,
				continue: event.continue,
				context: event.context,
			});
			return {
				entries: [{
					type: "custom",
					customType: "corrected-draft",
					data: { ok: true },
				}],
			};
		},
	},
};
