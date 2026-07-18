import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

export default function controlCompatExtension(pi: ExtensionAPI): void {
	pi.registerFlag("compat-enabled", {
		description: "Boolean compatibility flag",
		type: "boolean",
		default: false,
	});
	pi.registerFlag("compat-label", {
		description: "String compatibility flag",
		type: "string",
		default: "default",
	});

	pi.on("session_start", (_event, ctx) => {
		ctx.ui.notify(JSON.stringify({
			enabled: pi.getFlag("compat-enabled"),
			label: pi.getFlag("compat-label"),
		}), "info");
	});

	pi.registerShortcut("ctrl+k", {
		description: "First duplicate shortcut",
		async handler(ctx: ExtensionContext) {
			await ctx.ui.select("wrong shortcut", ["wrong"]);
		},
	});

	pi.registerShortcut("ctrl+e", {
		description: "Failing shortcut",
		handler() {
			throw new Error("shortcut fixture failure");
		},
	});
}
