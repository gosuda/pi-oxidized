/**
 * Replaced-session fixture: exercises newSession setup/withSession ordering,
 * cancel behaviour, and sendMessage/sendUserMessage on ReplacedSessionContext.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function replacedSessionExtension(pi: ExtensionAPI): void {
	pi.registerCommand("replacedSessionProbe", {
		description: "Exercise newSession setup + withSession + sendMessage/sendUserMessage",
		async handler(_args, ctx) {
			const report: Record<string, unknown> = {};

			// --- Success path: setup runs before withSession, both on non-cancelled ---
			const setupOrder: string[] = [];
			const result = await ctx.newSession({
				parentSession: "parent-1",
				setup: async (sessionManager) => {
					setupOrder.push("setup");
					// sessionManager is a proxy; record that it was received.
					report["setupReceived"] = sessionManager !== undefined;
				},
				withSession: async (replacedCtx) => {
					setupOrder.push("withSession");
					report["withSessionSendMessage"] = typeof replacedCtx.sendMessage;
					report["withSessionSendUserMessage"] = typeof replacedCtx.sendUserMessage;

					// Exercise sendMessage and sendUserMessage — these bridge to Rust
					// via sendSessionCommand (fire-and-forget session.command event).
					await replacedCtx.sendMessage({
						customType: "test-custom",
						content: "hello",
					});
					await replacedCtx.sendUserMessage("user hello");
				},
			});

			report["setupOrder"] = setupOrder;
			report["newSessionResult"] = result;

			ctx.ui.notify(JSON.stringify(report), "info");
		},
	});

	pi.registerCommand("replacedSessionCancel", {
		description: "Exercise newSession cancel: setup and withSession must NOT run",
		async handler(_args, ctx) {
			const report: Record<string, unknown> = {};

			const setupRan: string[] = [];
			const result = await ctx.newSession({
				parentSession: "parent-1",
				setup: async () => {
					setupRan.push("setup");
				},
				withSession: async () => {
					setupRan.push("withSession");
				},
			});

			report["setupRanOnCancel"] = setupRan;
			report["newSessionResult"] = result;

			ctx.ui.notify(JSON.stringify(report), "info");
		},
	});
}
