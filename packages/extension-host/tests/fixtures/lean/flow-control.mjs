function flow() {
	const current = globalThis.__leanFlow;
	if (current === undefined) throw new Error("lean flow fixture is not configured");
	return current;
}

export default {
	name: "lean-flow-control",
	tools: [
		{
			name: "many-updates",
			description: "Emit many synchronous partial results",
			execute: (_args, ctx) => {
				for (let index = 0; index < 200; index++) ctx.onUpdate({ index });
				flow()
					.late.promise.then(() => ctx.onUpdate({ index: "late" }))
					.catch(() => {
						// The late update lands after cancellation, where onUpdate
						// rejects by design; swallow it so the expected rejection
						// does not escape the fixture as an unhandled rejection.
					});
				return { ok: true };
			},
		},
		{
			name: "abort-updates",
			description: "Emit before and after cancellation",
			execute: async (_args, ctx) => {
				ctx.onUpdate({ index: "accepted" });
				await flow().abortGate.promise;
				ctx.onUpdate({ index: "rejected" });
				return { ok: true };
			},
		},
	],
	shortcuts: [
		{
			key: "ctrl+repeat",
			handler: ({ signal }) => flow().shortcut("repeat", signal),
		},
		{
			key: "ctrl+other",
			handler: ({ signal }) => flow().shortcut("other", signal),
		},
	],
};
