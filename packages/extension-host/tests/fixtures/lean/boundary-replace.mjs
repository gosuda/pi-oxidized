/**
 * Boundary fold fixture (later in load order). Proves replacement rather
 * than append: it overwrites the accumulated drafts wholesale and clears a
 * prior continuation with `continue: false`.
 */

function mark(name, value) {
	const key = "__leanEchoLog";
	const log = globalThis[key] ?? [];
	log.push({ name, value });
	globalThis[key] = log;
}

export default {
	name: "boundary-replace",
	hooks: {
		turn_end: (event) => {
			mark("replace.turn_end", {
				entries: event.entries,
				continue: event.continue,
				context: event.context,
			});
			return {
				entries: [{
					type: "custom_message",
					customType: "replace-draft",
					content: "replaced",
					display: true,
				}],
				continue: false,
			};
		},
	},
};
