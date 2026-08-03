import type { ExtensionAPI, ReplacedSessionContext } from "@earendil-works/pi-coding-agent";

export default function replacementReadyExtension(pi: ExtensionAPI): void {
	pi.registerCommand("replacementReadyProbe", {
		description: "Exercise replacementToken strip + ready emission on newSession",
		async handler(_args, ctx) {
			let replacementCtx: ReplacedSessionContext | undefined;
			const result = await ctx.newSession({
				parentSession: "parent-1",
				withSession: async (freshCtx) => {
					replacementCtx = freshCtx;
				},
			});
			if (replacementCtx === undefined) throw new Error("missing replacement context");
			replacementCtx.ui.notify(
				JSON.stringify({
					newSession: result,
					hasToken: Object.hasOwn(result, "replacementToken"),
				}),
				"info",
			);
		},
	});

	pi.registerCommand("replacementReadyCancel", {
		description: "Cancelled newSession must not emit replacementReady",
		async handler(_args, ctx) {
			const result = await ctx.newSession({ parentSession: "parent-1" });
			ctx.ui.notify(JSON.stringify({ newSession: result }), "info");
		},
	});

	pi.registerCommand("replacementReadyThrow", {
		description: "Handler throw after successful replacement still emits ready",
		async handler(_args, ctx) {
			await ctx.newSession({ parentSession: "parent-1" });
			throw new Error("post-replacement boom");
		},
	});

	pi.registerCommand("replacementReadyReload", {
		description: "Reload captures token and emits ready",
		async handler(_args, ctx) {
			await ctx.reload();
		},
	});

	pi.registerCommand("forkPassthroughProbe", {
		description: "Exercise fork selectedText passthrough + token strip",
		async handler(_args, ctx) {
			let replacementCtx: ReplacedSessionContext | undefined;
			const result = await ctx.fork("entry-1", {
				position: "at",
				withSession: async (freshCtx) => {
					replacementCtx = freshCtx;
				},
			});
			if (replacementCtx === undefined) throw new Error("missing replacement context");
			replacementCtx.ui.notify(
				JSON.stringify({
					fork: result,
					hasToken: Object.hasOwn(result, "replacementToken"),
				}),
				"info",
			);
		},
	});

	pi.registerCommand("navigateTreePassthroughProbe", {
		description: "Exercise navigateTree passthrough; must not emit replacementReady",
		async handler(_args, ctx) {
			const result = await ctx.navigateTree("leaf-1", { summarize: true });
			ctx.ui.notify(
				JSON.stringify({
					navigateTree: result,
					hasToken: Object.hasOwn(result, "replacementToken"),
				}),
				"info",
			);
		},
	});

	pi.registerCommand("concurrentIdleProbe", {
		description: "Idle command used as concurrent peer (no replacement)",
		async handler(_args, ctx) {
			await ctx.waitForIdle();
			ctx.ui.notify(JSON.stringify({ idle: true }), "info");
		},
	});
}
