import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

export default function controlCompatShadowExtension(pi: ExtensionAPI): void {
	pi.registerShortcut("ctrl+k", {
		description: "Last duplicate shortcut",
		async handler(ctx: ExtensionContext) {
			const selected = await ctx.ui.select("compat select", ["alpha", "beta"]);
			const confirmed = await ctx.ui.confirm("compat confirm", "continue?");
			const input = await ctx.ui.input("compat input", "type here");
			const edited = await ctx.ui.editor("compat editor", "draft");
			ctx.ui.notify(JSON.stringify({ selected, confirmed, input, edited }), "info");
		},
	});
}
