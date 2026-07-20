/**
 * Theme API fixture: exercises the ctx.ui theme surface end-to-end —
 * `theme` getter, `getAllThemes`, `getTheme`, and every `setTheme` form
 * (plain name, light/dark pair, unknown name, in-memory Theme object) —
 * and reports the observations as one JSON notify.
 */

import type { ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";

export default function themeApiExtension(pi: ExtensionAPI): void {
	pi.registerCommand("themeProbe", {
		description: "Exercise the ctx.ui theme API",
		async handler(_args, ctx) {
			const report: Record<string, unknown> = {};
			report["initial"] = ctx.ui.theme.name;

			const all = ctx.ui.getAllThemes();
			report["count"] = all.length;
			report["names"] = all.map((entry) => entry.name);
			report["allHavePaths"] = all.every((entry) => typeof entry.path === "string");

			const m3 = ctx.ui.getTheme("m3-dark");
			report["m3"] = m3 === undefined
				? undefined
				: { name: m3.name, accent: m3.getFgAnsi("accent") };
			report["missing"] = ctx.ui.getTheme("does-not-exist") === undefined;

			report["setClassic"] = ctx.ui.setTheme("classic-light");
			report["afterClassic"] = ctx.ui.theme.name;

			report["setPair"] = ctx.ui.setTheme("light/dark");
			report["afterPair"] = ctx.ui.theme.name;

			report["setMissing"] = ctx.ui.setTheme("nope");
			report["afterMissing"] = ctx.ui.theme.name;

			const inMemory = {
				name: "inmem",
				fg: (_color: string, text: string) => text,
				bg: (_color: string, text: string) => text,
				bold: (text: string) => text,
				italic: (text: string) => text,
				underline: (text: string) => text,
				inverse: (text: string) => text,
				strikethrough: (text: string) => text,
				getFgAnsi: () => "\x1b[38;2;1;2;3m",
				getBgAnsi: () => "\x1b[48;5;17m",
				getColorMode: () => "truecolor",
				getThinkingBorderColor: (_level: string) => (text: string) => text,
				getBashModeBorderColor: () => (text: string) => text,
			} as Theme;
			report["setObject"] = ctx.ui.setTheme(inMemory);
			report["final"] = ctx.ui.theme.name;

			ctx.ui.notify(JSON.stringify(report), "info");
		},
	});
}
