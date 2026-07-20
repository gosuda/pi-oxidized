/**
 * Session-actions fixture: exercises the bridged `ExtensionActions` /
 * `ExtensionContextActions` surface — fire-and-forget commands, mirrored
 * synchronous getters, the async `setModel` round-trip, correlated
 * `compact` callbacks, and the `ui.control` data-surface members — and
 * reports the observations as one JSON notify per probe command.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function sessionActionsExtension(pi: ExtensionAPI): void {
	pi.registerCommand("sessionProbe", {
		description: "Exercise the bridged session actions",
		async handler(_args, ctx) {
			const report: Record<string, unknown> = {};
			report["sessionName"] = pi.getSessionName();
			report["thinkingLevel"] = pi.getThinkingLevel();
			report["activeTools"] = pi.getActiveTools();
			report["allTools"] = pi.getAllTools().map((tool) => tool.name);
			report["commands"] = pi.getCommands().map((command) => command.name);
			report["isIdle"] = ctx.isIdle();
			report["hasPending"] = ctx.hasPendingMessages();
			report["systemPrompt"] = ctx.getSystemPrompt();
			report["contextUsage"] = ctx.getContextUsage();
			report["model"] = (ctx.model as { id?: string } | undefined)?.id;
			report["signal"] = ctx.signal === undefined ? "none" : "armed";

			pi.setSessionName("probe-renamed");
			pi.setLabel("entry-1", "flagged");
			pi.appendEntry("probe", { marker: 7 });
			pi.setActiveTools(["read"]);
			pi.setThinkingLevel("low");
			// Setter-then-getter coherence within one handler: the optimistic
			// local mirror must reflect the writes before any Rust re-push.
			report["nameAfterSet"] = pi.getSessionName();
			report["activeAfterSet"] = pi.getActiveTools();
			report["levelAfterSet"] = pi.getThinkingLevel();
			pi.sendMessage(
				{ customType: "probe", content: "hello", display: true },
				{ deliverAs: "nextTurn" },
			);
			pi.sendUserMessage("user text", { deliverAs: "followUp" });

			report["setModel"] = await pi.setModel({
				id: "probe-model",
				provider: "probe",
			} as Parameters<typeof pi.setModel>[0]);

			ctx.ui.notify(JSON.stringify(report), "info");
		},
	});

	pi.registerCommand("uiProbe", {
		description: "Exercise the bridged ui.control surface",
		async handler(_args, ctx) {
			const report: Record<string, unknown> = {};
			ctx.ui.setStatus("lint", "3 warnings");
			ctx.ui.setWorkingMessage("Crunching…");
			ctx.ui.setWorkingVisible(false);
			ctx.ui.setHiddenThinkingLabel("Pondering…");
			ctx.ui.setTitle("probe-title");
			ctx.ui.setEditorText("draft");
			report["editorAfterSet"] = ctx.ui.getEditorText();
			ctx.ui.pasteToEditor("+more");
			report["editorAfterPaste"] = ctx.ui.getEditorText();
			ctx.ui.setToolsExpanded(true);
			report["toolsExpanded"] = ctx.ui.getToolsExpanded();
			ctx.ui.setFooter(() => ({ render: () => ["custom footer"] }));
			ctx.ui.setHeader(() => ({ render: () => ["custom header"] }));
			ctx.ui.notify(JSON.stringify(report), "info");
		},
	});

	pi.registerCommand("compactProbe", {
		description: "Exercise the correlated compact bridge",
		async handler(_args, ctx) {
			await new Promise<void>((resolve) => {
				ctx.compact({
					customInstructions: "keep decisions",
					onComplete: (result) => {
						ctx.ui.notify(JSON.stringify({ compact: "ok", result }), "info");
						resolve();
					},
					onError: (error) => {
						ctx.ui.notify(JSON.stringify({ compact: "err", message: error.message }), "info");
						resolve();
					},
				});
			});
		},
	});
}

export function abortProbeFactory(pi: ExtensionAPI): void {
	pi.registerCommand("abortProbe", {
		description: "Exercise the abort signal bridge",
		async handler(_args, ctx) {
			const signal = ctx.signal;
			const before = signal === undefined ? "none" : String(signal.aborted);
			ctx.abort();
			const after = signal === undefined ? "none" : String(signal.aborted);
			ctx.ui.notify(JSON.stringify({ before, after }), "info");
		},
	});
}
