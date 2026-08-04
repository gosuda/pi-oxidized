/**
 * Replaced-session fixture: exercises newSession setup/withSession ordering,
 * the narrow SessionManager bridge (supported mutation + unsupported throw),
 * cancel behaviour, and sendMessage/sendUserMessage on ReplacedSessionContext.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

/** Minimal structural setup surface needed by this fixture's `setup` callback. */
type SessionManagerSetup = {
	appendCustomEntry(customType: string, data?: unknown): Promise<void>;
	appendSessionInfo(name: string): Promise<void>;
	getSessionName(): string | undefined;
	getEntries(): unknown;
};

export default function replacedSessionExtension(pi: ExtensionAPI): void {
	pi.registerCommand("replacedSessionProbe", {
		description: "Exercise newSession setup + withSession + sendMessage/sendUserMessage",
		async handler(_args, ctx) {
			const report: Record<string, unknown> = {};

			// --- Success path: setup runs before withSession, both on non-cancelled ---
			const setupOrder: string[] = [];
			const result = await ctx.newSession({
				parentSession: "parent-1",
				setup: async (sessionManager: SessionManagerSetup) => {
					setupOrder.push("setup");
					// sessionManager is the narrow bridge proxy.
					report["setupReceived"] = sessionManager !== undefined;

					// Supported mutations route through the bridge and await wire delivery.
					// Neither operation fabricates the reference SessionManager entry ID.
					await sessionManager.appendCustomEntry("setup-custom", { from: "setup" });
					await sessionManager.appendSessionInfo("setup-session");

					// This is the one SessionManager getter mirrored by the host.
					report["setupSessionName"] = sessionManager.getSessionName();
					// Unsupported methods are on the minimal bridge surface for probe
					// coverage but throw at runtime when invoked.
					try {
						sessionManager.getEntries();
						report["unsupportedThrew"] = false;
					} catch (e) {
						report["unsupportedThrew"] = true;
						report["unsupportedMessage"] = (e as Error).message;
					}
				},
				withSession: async (replacedCtx) => {
					setupOrder.push("withSession");
					report["withSessionSendMessage"] = typeof replacedCtx.sendMessage;
					report["withSessionSendUserMessage"] = typeof replacedCtx.sendUserMessage;

					// Exercise sendMessage and sendUserMessage — these bridge to
					// Rust via sendSessionCommand and await the wire write.
					await replacedCtx.sendMessage({
						customType: "test-custom",
						content: "hello",
						display: true,
					});
					await replacedCtx.sendUserMessage("user hello");
					report["withSessionSendsDone"] = true;

				// Notify through the fresh replacement context; the
				// original ctx is stale after a successful replacement.
				report["setupOrder"] = setupOrder;
				// `cancelled: false` here is the protocol precondition
				// established by entering withSession (a cancelled
				// replacement never calls this callback), not a measured
				// result read back from the wire.
				report["newSessionResult"] = { cancelled: false };
				replacedCtx.ui.notify(JSON.stringify(report), "info");
				},
			});

			// Retain the original ctx only when replacement is cancelled.
			if (result.cancelled) {
				report["setupOrder"] = setupOrder;
				report["newSessionResult"] = result;
				ctx.ui.notify(JSON.stringify(report), "info");
			}
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
