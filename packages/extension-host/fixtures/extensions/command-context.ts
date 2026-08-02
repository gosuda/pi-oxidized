/**
 * Command-context fixture: exercises ExtensionCommandContext methods
 * (waitForIdle / newSession) plus mirrored getContextUsage / scopedModels.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function commandContextExtension(pi: ExtensionAPI): void {
	pi.registerCommand("commandContextProbe", {
		description: "Exercise command context + mirrored session getters",
		async handler(_args, ctx) {
			const report: Record<string, unknown> = {
				contextUsage: ctx.getContextUsage(),
				scopedModels: ctx.scopedModels,
				hasWaitForIdle: typeof ctx.waitForIdle === "function",
				hasNewSession: typeof ctx.newSession === "function",
			};

			await ctx.waitForIdle();
			report["waitForIdleOk"] = true;

			const result = await ctx.newSession({ parentSession: "parent-1" });
			report["newSession"] = result;

			ctx.ui.notify(JSON.stringify(report), "info");
		},
	});
}
