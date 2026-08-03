import type { ExtensionAPI, ExtensionCommandContext } from "@earendil-works/pi-coding-agent";

type SessionManagerSetup = {
	appendSessionInfo(name: string): Promise<void>;
};

let capturedCtx: ExtensionCommandContext | undefined;

export function resetCapturedCtx(): void {
	capturedCtx = undefined;
}

export default function staleCtxExtension(pi: ExtensionAPI): void {
	pi.registerCommand("captureCtx", {
		description: "Capture ctx for later staleness test",
		async handler(_args, ctx) {
			capturedCtx = ctx;
			ctx.ui.notify(JSON.stringify({ captured: true }), "info");
		},
	});

	pi.registerCommand("useStaleCtx", {
		description: "Use previously captured ctx after a runner rebuild",
		async handler() {
			if (capturedCtx === undefined) throw new Error("no captured ctx");
			await capturedCtx.waitForIdle();
		},
	});

	pi.registerCommand("staleNewSession", {
		description: "A successful newSession makes this ctx stale",
		async handler(_args, ctx) {
			await ctx.newSession({ parentSession: "parent-1" });
			await ctx.newSession({ parentSession: "parent-2" });
		},
	});

	pi.registerCommand("staleFork", {
		description: "A successful fork makes this ctx stale",
		async handler(_args, ctx) {
			await ctx.fork("entry-1", { position: "at" });
			await ctx.fork("entry-2", { position: "at" });
		},
	});

	pi.registerCommand("staleSwitchSession", {
		description: "A successful switchSession makes this ctx stale",
		async handler(_args, ctx) {
			await ctx.switchSession("/tmp/first.jsonl");
			await ctx.switchSession("/tmp/second.jsonl");
		},
	});

	pi.registerCommand("staleReload", {
		description: "A successful reload makes this ctx stale",
		async handler(_args, ctx) {
			await ctx.reload();
			await ctx.reload();
		},
	});

	pi.registerCommand("cancelledCtxRemainsUsable", {
		description: "A cancelled replacement leaves this ctx usable",
		async handler(_args, ctx) {
			const result = await ctx.newSession({ parentSession: "cancelled" });
			await ctx.waitForIdle();
			ctx.ui.notify(JSON.stringify({ cancelled: result.cancelled, stillUsable: true }), "info");
		},
	});

	pi.registerCommand("withSessionUsesFreshContext", {
		description: "setup and withSession work after the original ctx turns stale",
		async handler(_args, ctx) {
			await ctx.newSession({
				setup: async (sessionManager: SessionManagerSetup) => {
					await sessionManager.appendSessionInfo("setup-after-token");
				},
				withSession: async (replacedCtx) => {
					await replacedCtx.sendUserMessage("from fresh replacement ctx");
				},
			});
			await ctx.waitForIdle();
		},
	});

	pi.registerCommand("capturedMethodStalesAfterNewSession", {
		description: "Method captured before newSession must throw after replacement",
		async handler(_args, ctx) {
			const waitForIdle = ctx.waitForIdle;
			await ctx.newSession({
				withSession: async (replacedCtx) => {
					await replacedCtx.sendUserMessage("from fresh replacement ctx");
				},
			});
			await waitForIdle();
		},
	});

	pi.registerCommand("activeCtxWorks", {
		description: "A different command gets a usable context",
		async handler(_args, ctx) {
			await ctx.waitForIdle();
			ctx.ui.notify(JSON.stringify({ active: true }), "info");
		},
	});
}
