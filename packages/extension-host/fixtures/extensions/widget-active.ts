/**
 * Active widget extension: pushes a height-changing widget on session_start.
 * Used for the 20-active-widget scaling branch.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

let counter = 0;

export default function widgetActiveExtension(pi: ExtensionAPI): void {
	pi.on("session_start", (_event, ctx) => {
		if (!ctx.hasUI) return;
		const n = counter++;
		const lines = [`widget-${n}`, `line-a-${n}`, `line-b-${n}`];
		ctx.ui.setWidget(`widget.active.${n}`, lines);
	});
}
